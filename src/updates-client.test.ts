import { describe,expect,it,vi } from "vitest";
import { createUpdatesController,parseUpdateObservation,parseUpdatesStatus,UPDATE_TARGETS } from "./updates-client";
import type { UpdatesClient,UpdatesStatus,UpdateReceipt } from "./updates-client";
export const ID="11111111-1111-4111-8111-111111111111";
export const UPDATE_RECEIPT:UpdateReceipt={id:ID,config_revision:1,metadata:{version:"1.0.1",sequence:1,target:"linux-x86_64",bytes:42,sha256:"a".repeat(64),manifest_sha256:"b".repeat(64)},status:"verified",stage_authority:false,reason:null,created_at_ms:1,updated_at_ms:2};
export const UPDATE_STATUS:UpdatesStatus={mode:"verification_only",installed:false,automatic_update_enabled:false,configuration:{revision:1,key_id:"fixture-key",source:"https://updates.example.invalid/stable/",public_key:Array(32).fill(0),fingerprint:"c".repeat(64),enabled:true},floors:UPDATE_TARGETS.map(target=>({target,version:"0.0.0",sequence:0,manifest_sha256:null,sha256:null})),receipts:[UPDATE_RECEIPT],staging:{files:1,bytes:42},blocked_reason:null};
const deferred=<T,>()=>{let resolve!:(value:T)=>void;const promise=new Promise<T>(r=>resolve=r);return {promise,resolve};};
describe("strict verifier observations",()=>{
  it("separates historical receipt from rechecked current bytes and rejects installer claims",()=>{
    expect(parseUpdatesStatus(UPDATE_STATUS).receipts[0].stage_authority).toBe(false);
    for(const change of [{installed:true},{automatic_update_enabled:true},{mode:"verification_disabled"},{blocked_reason:"staging_unavailable"},{receipts:[{...UPDATE_RECEIPT,stage_authority:true}]}])expect(()=>parseUpdatesStatus({...UPDATE_STATUS,...change})).toThrow();
    const observation={receipt:{...UPDATE_RECEIPT,stage_authority:true},current_stage_verified:true,installed:false,automatic_update_enabled:false};
    expect(parseUpdateObservation(observation,ID).current_stage_verified).toBe(true);
    expect(()=>parseUpdateObservation(observation,"22222222-2222-4222-8222-222222222222")).toThrow();
    expect(()=>parseUpdateObservation({...observation,current_stage_verified:false},ID)).toThrow();
  });
  it("bounds histories, byte counts, target sets, trust shape and unknown fields",()=>{
    for(const change of [{receipts:Array(33).fill(UPDATE_RECEIPT)},{staging:{files:513,bytes:0}},{staging:{files:1,bytes:536870913}},{floors:Array(6).fill(UPDATE_STATUS.floors[0])},{configuration:{...UPDATE_STATUS.configuration,source:"https://user:secret@example.invalid/"}},{private_path:"canary"}])expect(()=>parseUpdatesStatus({...UPDATE_STATUS,...change})).toThrow();
    expect(()=>parseUpdatesStatus({...UPDATE_STATUS,receipts:[{...UPDATE_RECEIPT,reason:"secret-canary"}]})).toThrow();
  });
  it("stale and disposed reads never replace newer navigation; never calls a writer",async()=>{
    const old=deferred<unknown>(),changed=vi.fn();
    const client:UpdatesClient={status:vi.fn().mockReturnValueOnce(old.promise).mockResolvedValue({...UPDATE_STATUS,mode:"verification_disabled",configuration:null}),receipt:vi.fn()};
    const controller=createUpdatesController(client,changed);const first=controller.refresh();await controller.refresh();old.resolve(UPDATE_STATUS);await first;
    expect(controller.getState().status?.mode).toBe("verification_disabled");
    const late=deferred<unknown>();vi.mocked(client.status).mockReturnValue(late.promise);const read=controller.refresh();controller.dispose();const count=changed.mock.calls.length;late.resolve(UPDATE_STATUS);await read;expect(changed).toHaveBeenCalledTimes(count);
  });
  it("refresh invalidates a previous positive stage observation even before disabled or blocked status returns",async()=>{
    for(const status of [{...UPDATE_STATUS,mode:"verification_disabled",configuration:{...UPDATE_STATUS.configuration,enabled:false,revision:2}}, {...UPDATE_STATUS,mode:"verification_blocked",blocked_reason:"witness_unavailable",staging:null}]) {
      const pending=deferred<unknown>();const client:UpdatesClient={status:vi.fn().mockReturnValue(pending.promise),receipt:vi.fn().mockResolvedValue({receipt:{...UPDATE_RECEIPT,stage_authority:true},current_stage_verified:true,installed:false,automatic_update_enabled:false})};
      const controller=createUpdatesController(client,()=>{});await controller.inspect(ID);expect(controller.getState().selected?.current_stage_verified).toBe(true);
      const refresh=controller.refresh();expect(controller.getState().selected).toBeNull();pending.resolve(status);await refresh;expect(controller.getState().selected).toBeNull();expect(controller.getState().status?.mode).toBe(status.mode);controller.dispose();
    }
  });
  it("malformed/wrong-ID receipt stays unconfirmed and errors never echo input",async()=>{
    const client:UpdatesClient={status:vi.fn().mockRejectedValue(new Error("private-canary")),receipt:vi.fn().mockResolvedValue({receipt:UPDATE_RECEIPT,current_stage_verified:true,installed:false,automatic_update_enabled:false})};
    const controller=createUpdatesController(client,()=>{});await controller.refresh();expect(controller.getState().notice).not.toContain("private-canary");await controller.inspect(ID);expect(controller.getState().selected).toBeNull();expect(controller.getState().notice).toContain("不要重发");await controller.inspect("../arbitrary");expect(client.receipt).toHaveBeenCalledTimes(1);controller.dispose();
  });
});
