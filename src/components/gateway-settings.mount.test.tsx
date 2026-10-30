// @vitest-environment jsdom
import {act} from "react";
import {createRoot} from "react-dom/client";
import {afterEach,describe,expect,it,vi} from "vitest";
import {GatewaySettings} from "./gateway-settings";
import {EMPTY_GATEWAY} from "../gateway-client";
import type {GatewayClient,GatewayCredentialClient,GatewaySnapshot} from "../gateway-client";
Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true});
const ID="11111111-1111-4111-8111-111111111111";
const snapshot:GatewaySnapshot={...EMPTY_GATEWAY,configuration:{id:ID,revision:1,device_id:ID,origin:"https://persisted.example.invalid/",enabled:false,credential_state:"missing",mode:"fixture"}};
let cleanup:(()=>Promise<void>)|undefined;
afterEach(async()=>{await cleanup?.();cleanup=undefined;});
describe("Gateway settings actual mount",()=>{
 it("finishes its initial owner read without manual refresh, and fences replacement/closed reads",async()=>{
  let resolve!:(value:GatewaySnapshot)=>void;
  const pending=new Promise<GatewaySnapshot>(ok=>{resolve=ok;});
  const api:GatewayClient={snapshot:vi.fn().mockReturnValue(pending),command:vi.fn(),receipt:vi.fn()};
  const credentials:GatewayCredentialClient={setCredential:vi.fn(),deleteCredential:vi.fn(),cleanupCredential:vi.fn(),operation:vi.fn()};
  const container=document.createElement("div");document.body.append(container);const root=createRoot(container);
  cleanup=async()=>{await act(async()=>root.unmount());container.remove();};
  const render=(initialSnapshot:GatewaySnapshot)=>root.render(<GatewaySettings initialSnapshot={initialSnapshot} client={api} credentialClient={credentials} sessions={[]} executor="native"/>);
  await act(async()=>render(EMPTY_GATEWAY));
  expect(api.snapshot).toHaveBeenCalledTimes(1);
  const signal=vi.mocked(api.snapshot).mock.calls[0][0];expect(signal?.aborted).toBe(false);
  await act(async()=>resolve(snapshot));
  expect((container.querySelector('input[placeholder="https://agent.example.invalid"]') as HTMLInputElement).value).toBe(snapshot.configuration!.origin);
  await act(async()=>render(EMPTY_GATEWAY));
  // Identical initial input is not a new authoritative replacement.
  expect(container.textContent).toContain("保存新配置修订");
  await act(async()=>render({...EMPTY_GATEWAY}));
  expect(container.textContent).toContain("停用创建网关配置");
  let late!:(value:GatewaySnapshot)=>void;vi.mocked(api.snapshot).mockReturnValue(new Promise(ok=>{late=ok;}));
  await act(async()=>{(Array.from(container.querySelectorAll("button")).find(b=>b.textContent==="刷新状态") as HTMLButtonElement).click();});
  const finalSignal=vi.mocked(api.snapshot).mock.calls[1][0];
  await cleanup!();cleanup=undefined;expect(finalSignal?.aborted).toBe(true);
  await act(async()=>late(snapshot));expect(container.isConnected).toBe(false);
 });
});
