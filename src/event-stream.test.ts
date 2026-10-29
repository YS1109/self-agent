import { afterEach, describe, expect, it, vi } from "vitest";
import { parseRunEvent, SseDecoder, subscribeEvents, type StreamMessage } from "./event-stream";
import { EMPTY_SNAPSHOT, type RunEvent } from "./types";
const native = vi.hoisted(() => ({ invoke:vi.fn(),channels:[] as {onmessage:(message:StreamMessage)=>void}[] }));
vi.mock("@tauri-apps/api/core",()=>({ invoke:native.invoke,Channel:class { onmessage=(_message:StreamMessage)=>{}; constructor(){native.channels.push(this);} } }));
afterEach(()=>{vi.unstubAllGlobals();vi.clearAllMocks();native.channels.length=0;});
const event=(sequence:number,text="中文"):RunEvent=>({id:`e${sequence}`,sequence,task_id:"t",kind:"assistant_delta",message:"回复增量",created_at:"now",data:{kind:"assistant_delta",text}});
const wire=(sequence:number)=>`event: run_event\nid: ${sequence}\ndata: ${JSON.stringify(event(sequence))}\n\n`;
function response(text:string,closed=false) { return new Response(new ReadableStream({ start(controller){controller.enqueue(new TextEncoder().encode(text));if(closed)controller.close();} }),{headers:{"Content-Type":"text/event-stream"}}); }

describe("SSE分片解析",()=>{
 it("支持中文每字节边界、CRLF、CR以及多行data",()=>{
  const data=new TextEncoder().encode(`: keepalive\r\n\r\nevent: run_event\r\nid: 1\r\ndata: ${JSON.stringify(event(1))}\r\n\r\n`);
  for(let split=0;split<=data.length;split++){
   const parser=new SseDecoder();const frames=[...parser.push(data.slice(0,split)),...parser.push(data.slice(split))];
   expect(frames).toHaveLength(1);expect(parseRunEvent(frames[0]).data).toEqual({kind:"assistant_delta",text:"中文"});
  }
  const parser=new SseDecoder();expect(parser.push(new TextEncoder().encode("event: resync\rdata: one\rdata: two\r\r"))[0].data).toBe("one\ntwo");
 });
 it("不发布未完成帧，限制大小，拒绝错误UTF8和游标",()=>{
  const parser=new SseDecoder();expect(parser.push(new TextEncoder().encode("event: run_event\ndata: incomplete"))).toEqual([]);
  expect(()=>new SseDecoder().push(new Uint8Array([255]))).toThrow();
  expect(()=>new SseDecoder().push(new TextEncoder().encode("x".repeat(512*1024+1)))).toThrow();
  expect(()=>parseRunEvent({event:"run_event",id:"2",data:JSON.stringify(event(1))})).toThrow();
  expect(()=>parseRunEvent({event:"run_event",id:"9007199254740993",data:"{}"})).toThrow();
  expect(()=>parseRunEvent({event:"run_event",id:"1",data:JSON.stringify({...event(1),data:{kind:"assistant_delta",text:{invalid:true}}})})).toThrow();
 });
});

describe("桌面事件桥生命周期",()=>{
 it("只经Rust Channel收事件，应用后ack，卸载后晚到消息不影响界面",async()=>{
  vi.stubGlobal("window",{__TAURI_INTERNALS__:{}});const fetcher=vi.fn();vi.stubGlobal("fetch",fetcher);native.invoke.mockResolvedValue(undefined);
  const received=vi.fn();const stop=subscribeEvents(4,received);
  await vi.waitFor(()=>expect(native.invoke).toHaveBeenCalledWith("start_event_stream",expect.objectContaining({after:4,channel:expect.anything()})));
  const call=native.invoke.mock.calls.find(([name])=>name==="start_event_stream")!;const id=call[1].subscriptionId;
  native.channels[0].onmessage({type:"event",event:event(5),delivery_id:1});
  expect(received).toHaveBeenCalledTimes(1);expect(native.invoke).toHaveBeenCalledWith("ack_event_stream",{subscriptionId:id,deliveryId:1});
  native.channels[0].onmessage({type:"status",state:"connected",delivery_id:2});
  expect(native.invoke).toHaveBeenCalledWith("ack_event_stream",{subscriptionId:id,deliveryId:2});
  received.mockClear();
  stop();native.channels[0].onmessage({type:"event",event:event(6),delivery_id:2});
  expect(received).not.toHaveBeenCalled();expect(native.invoke).toHaveBeenCalledWith("stop_event_stream",{subscriptionId:id});
  expect(fetcher).not.toHaveBeenCalled();expect(JSON.stringify(native.invoke.mock.calls)).not.toMatch(/bearer|authorization|token/i);
 });
 it("注册尚未完成时卸载，注册返回后再次停止；新订阅不受旧cleanup影响",async()=>{
  vi.stubGlobal("window",{__TAURI_INTERNALS__:{}});let resolve!:()=>void;
  native.invoke.mockImplementation((name:string)=>name==="start_event_stream"?new Promise<void>(r=>{resolve=r;}):Promise.resolve());
  const old=vi.fn();const stop=subscribeEvents(0,old);
  await vi.waitFor(()=>expect(native.channels).toHaveLength(1));stop();resolve();
  await vi.waitFor(()=>expect(native.invoke.mock.calls.filter(([name])=>name==="stop_event_stream")).toHaveLength(2));
  native.invoke.mockResolvedValue(undefined);const current=vi.fn();const stopCurrent=subscribeEvents(0,current);
  await vi.waitFor(()=>expect(native.channels).toHaveLength(2));
  native.channels[0].onmessage({type:"event",event:event(1),delivery_id:1});native.channels[1].onmessage({type:"event",event:event(1),delivery_id:1});
  expect(old).not.toHaveBeenCalled();expect(current).toHaveBeenCalledTimes(1);
  const ids=native.invoke.mock.calls.filter(([name])=>name==="start_event_stream").map(([,args])=>args.subscriptionId);expect(ids[0]).not.toBe(ids[1]);stopCurrent();
 });
 it("立即卸载不启动异步事件注册",async()=>{
  vi.stubGlobal("window",{__TAURI_INTERNALS__:{}});const stop=subscribeEvents(0,vi.fn());stop();await Promise.resolve();await Promise.resolve();expect(native.invoke).not.toHaveBeenCalled();
 });
});

describe("浏览器受控事件代理",()=>{
 it("断线按最后已应用游标重连并去重，cleanup中止订阅",async()=>{
  vi.stubGlobal("window",{});const fetcher=vi.fn().mockResolvedValueOnce(response(wire(1),true)).mockResolvedValue(response(wire(1)+wire(2)));
  vi.stubGlobal("fetch",fetcher);const messages:StreamMessage[]=[];const stop=subscribeEvents(0,message=>messages.push(message));
  await vi.waitFor(()=>expect(messages.filter(m=>m.type==="event")).toHaveLength(2),{timeout:2000});
  expect(fetcher.mock.calls[1][0]).toBe("/api/v1/events?after=1");
  const ids=messages.filter(m=>m.type==="event").map(m=>m.event.sequence);expect(ids).toEqual([1,2]);
  stop();expect(fetcher.mock.calls[0][1].signal.aborted).toBe(true);
  expect(JSON.stringify(fetcher.mock.calls)).not.toMatch(/bearer|authorization/i);
 });
 it("409恢复快照后替换游标，不拼接快照文本",async()=>{
  vi.stubGlobal("window",{});const fetcher=vi.fn().mockResolvedValueOnce(new Response("{}",{status:409}))
   .mockResolvedValueOnce(new Response(JSON.stringify({...EMPTY_SNAPSHOT,event_cursor:10})))
   .mockResolvedValue(response(wire(11)));
  vi.stubGlobal("fetch",fetcher);const messages:StreamMessage[]=[];const stop=subscribeEvents(1,m=>messages.push(m));
  await vi.waitFor(()=>expect(messages.some(m=>m.type==="event")).toBe(true));stop();
  expect(fetcher.mock.calls[1][0]).toBe("/api/v1/snapshot?window=1");expect(fetcher.mock.calls[2][0]).toBe("/api/v1/events?after=10");
  const data=messages.filter(m=>m.type!=="status");expect(data[0].type).toBe("snapshot");expect(data[1].type).toBe("event");
 });
});

describe("上下文估算事件校验", () => {
  it("只接受有界数值，未知新类型仍可兼容", () => {
    const data={kind:"context_estimate",estimated_input_tokens:1200,context_window:128000,compression_threshold:80,source:"codex_current_input_only",phase:"before_turn"};
    const frame={event:"run_event",id:"1",data:JSON.stringify({...event(1),data})};
    expect(parseRunEvent(frame).data).toEqual(data);
    for(const invalid of [{...data,estimated_input_tokens:-1},{...data,context_window:0},{...data,compression_threshold:101},{...data,source:2}])
      expect(()=>parseRunEvent({...frame,data:JSON.stringify({...event(1),data:invalid})})).toThrow("上下文估算");
    expect(parseRunEvent({...frame,data:JSON.stringify({...event(1),data:{kind:"future_kind"}})}).sequence).toBe(1);
  });
});
