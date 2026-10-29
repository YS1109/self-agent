// @vitest-environment jsdom
import {act} from "react";
import {createRoot} from "react-dom/client";
import {renderToStaticMarkup} from "react-dom/server";
import {describe,it,expect,vi} from "vitest";
import {UpdatesSettings} from "./updates-settings";
import {UPDATE_TARGETS} from "../updates-client";
import type {UpdatesClient,UpdatesStatus} from "../updates-client";
const status:UpdatesStatus={mode:"verification_only",installed:false,automatic_update_enabled:false,configuration:{revision:3,key_id:"confirmed-key",source:"https://updates.example.invalid/",public_key:Array(32).fill(0),fingerprint:"a".repeat(64),enabled:true},floors:UPDATE_TARGETS.map(target=>({target,version:"0.0.0",sequence:0,manifest_sha256:null,sha256:null})),receipts:[],staging:{files:0,bytes:0},blocked_reason:null};
describe("owner verifier status",()=>{
  it("mount completes its initial read without manual refresh and exposes no install action",async()=>{
    Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true});
    const client:UpdatesClient={status:vi.fn().mockResolvedValue(status),receipt:vi.fn()};
    const node=document.createElement("div"),root=createRoot(node);
    await act(async()=>{root.render(<UpdatesSettings client={client}/>);});
    expect(client.status).toHaveBeenCalledTimes(1);expect(node.textContent).toContain("confirmed-key");expect(node.textContent).toContain("仅验证信任已配置");expect(node.textContent).toContain("生产自动更新与安装始终禁用");
    expect([...node.querySelectorAll("button")].map(b=>b.textContent)).toEqual(["刷新验证状态","只读复核暂存状态"]);
    await act(async()=>root.unmount());
  });
  it("SSR does not enroll trust, upload or assume configured production identity",()=>{
    const client:UpdatesClient={status:vi.fn(),receipt:vi.fn()};const html=renderToStaticMarkup(<UpdatesSettings client={client}/>);
    expect(html).toContain("临时测试密钥");expect(html).toContain("不会联网下载");expect(html).toContain("不取消后台已接收");expect(client.status).not.toHaveBeenCalled();expect(client.receipt).not.toHaveBeenCalled();
  });
  it("unmount fences late server replies",async()=>{
    Object.assign(globalThis,{IS_REACT_ACT_ENVIRONMENT:true});
    let resolve!:(v:unknown)=>void;const pending=new Promise(r=>resolve=r);const client:UpdatesClient={status:vi.fn().mockReturnValue(pending),receipt:vi.fn()};
    const node=document.createElement("div"),root=createRoot(node);await act(async()=>root.render(<UpdatesSettings client={client}/>));
    const signal=vi.mocked(client.status).mock.calls[0][0];await act(async()=>root.unmount());expect(signal.aborted).toBe(true);await act(async()=>resolve(status));expect(node.childNodes).toHaveLength(0);
  });
});
