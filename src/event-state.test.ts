import { describe, expect, it } from "vitest";
import { createEventState, receiveEvent, receiveSnapshot, taskTools } from "./event-state";
import { EMPTY_SNAPSHOT, DESKTOP_SOURCE, type RunEvent, type Snapshot, type TaskRun } from "./types";
const task: TaskRun = { id:"t", session_id:"s", title:"目标",status:"running",execution_mode:"native",created_at:"now",updated_at:"now",source:DESKTOP_SOURCE,steps:[],task_kind:"main" };
const snapshot = (cursor: number, text="", tasks=[task]): Snapshot => ({...EMPTY_SNAPSHOT,event_cursor:cursor,tasks:tasks.map(t=>({...t,stream_output:text}))});
const delta = (sequence:number,text:string,task_id="t"):RunEvent => ({id:`e${sequence}`,sequence,task_id,kind:"assistant_delta",message:"回复增量",created_at:"now",data:{kind:"assistant_delta",text}});

describe("持久事件与快照合并",()=>{
 it("replay预算耗尽时不推进应用游标，权威快照覆盖缺口后才恢复",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(0));
  for(let n=1;n<=512;n++)s=receiveEvent(s,delta(n,"x"));
  const before=s.snapshot.tasks[0].stream_output;
  s=receiveEvent(s,delta(513,"不能默默跳过"));
  expect(s.needsSnapshot).toBe(true);expect(s.cursor).toBe(512);expect(s.recoveryFloor).toBe(513);expect(s.snapshot.tasks[0].stream_output).toBe(before);
  expect(receiveSnapshot(s,snapshot(512,"旧快照"))).toBe(s);
  s=receiveSnapshot(s,snapshot(513,"包含完整缺口的权威结果"));expect(s.needsSnapshot).toBe(false);expect(s.cursor).toBe(513);expect(s.pending).toEqual([]);
 });
 it("实时事件字节与pending双重受限，不让大工具输出形成150MiB缓存",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(0));
  for(let n=1;n<50;n++)s=receiveEvent(s,{...delta(n,""),kind:"tool_finished",data:{kind:"tool_finished",call_id:String(n),name:"shell",success:true,output:"x".repeat(256*1024)}});
  expect(s.needsSnapshot).toBe(true);expect(s.pendingBytes).toBeLessThanOrEqual(2*1024*1024);expect(s.eventBytes).toBeLessThanOrEqual(2*1024*1024);expect(s.snapshotBytes).toBeLessThanOrEqual(16*1024*1024);
 });
 it("新daemon epoch替换旧cursor与窗口元数据，缺少集合或伪计数拒绝",()=>{
  const withHistory=(anchor:string,cursor:number):Snapshot=>{
    const base={...snapshot(cursor),execution_requests:[],context_states:[]};
    const fields=["sessions","tasks","events","deliveries","selections","plans","plan_changes","execution_requests","context_states"] as const;
    return {...base,history:{version:1,anchor,collections:Object.fromEntries(fields.map(key=>[key,{total:base[key].length,returned:base[key].length,has_more:false,pending:0}])),session_messages:{}}};
  };
  const first=withHistory("10000000-0000-4000-8000-000000000001",100),second=withHistory("20000000-0000-4000-8000-000000000002",1);
  let s=receiveSnapshot(createEventState(),first);s=receiveSnapshot(s,second);expect(s.cursor).toBe(1);expect(s.snapshot.history).toEqual(second.history);
  expect(()=>receiveSnapshot(s,{...second,history:{...second.history!,collections:{}}})).toThrow("元数据");
  expect(()=>receiveSnapshot(s,{...second,history:{...second.history!,collections:{...second.history!.collections,tasks:{total:100,returned:0,has_more:true}}}})).toThrow("计数");
  expect(()=>receiveSnapshot(s,{...second,tasks:Array.from({length:301},()=>task)})).toThrow("数量限制");
 });
 it("按全局游标去重，跨任务推进，不按文本去重",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(2));
  s=receiveEvent(s,delta(3,"你")); s=receiveEvent(s,delta(3,"你"));
  s=receiveEvent(s,delta(7,"other","other"));s=receiveEvent(s,delta(8,"你"));
  expect(s.cursor).toBe(8);expect(s.snapshot.tasks[0].stream_output).toBe("你你");
 });
 it("迟到快照重放更新事件，替换累计正文而非重复追加",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(1,"A"));
  s=receiveEvent(s,delta(2,"B"));s=receiveEvent(s,delta(3,"C"));
  s=receiveSnapshot(s,snapshot(2,"AB"));expect(s.snapshot.tasks[0].stream_output).toBe("ABC");
  s=receiveSnapshot(s,snapshot(3,"ABC"));expect(s.snapshot.tasks[0].stream_output).toBe("ABC");
  expect(receiveSnapshot(s,snapshot(1,"A"))).toBe(s);
 });
 it("任务创建快照晚于事件时保留事件，看到任务后准确重放",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(1,"",[]));
  s=receiveEvent(s,delta(3,"新任务"));
  s=receiveSnapshot(s,snapshot(2));expect(s.snapshot.tasks[0].stream_output).toBe("新任务");
 });
 it("resync以权威累计前缀替换，清理旧pending",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(1,"old"));s=receiveEvent(s,delta(2," text"));
  s=receiveSnapshot(s,snapshot(100,"恢复后的完整回复"));
  expect(s.snapshot.tasks[0].stream_output).toBe("恢复后的完整回复");expect(s.pending).toEqual([]);
  s=receiveEvent(s,delta(101,"！"));expect(s.snapshot.tasks[0].stream_output).toBe("恢复后的完整回复！");
 });
 it("有界pending拒绝无法重放的旧快照，后续新快照恢复正常",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(1));
  for(let n=2;n<600;n++)s=receiveEvent(s,delta(n,"a"));
  expect(s.pending.length).toBeLessThanOrEqual(512);expect(s.snapshot.events.length).toBe(300);
  expect(receiveSnapshot(s,snapshot(2,"a"))).toBe(s);
  s=receiveSnapshot(s,snapshot(600,"权威回复"));expect(s.snapshot.tasks[0].stream_output).toBe("权威回复");
 });
 it("中文截断不切断码点，终态拒绝迟到回复及状态回退",()=>{
  let s=receiveSnapshot(createEventState(),snapshot(1,"中".repeat(87381)));
  s=receiveEvent(s,delta(2,"文"));expect(s.snapshot.tasks[0].stream_output_truncated).toBe(true);
  expect(s.snapshot.tasks[0].stream_output).not.toContain("�");
  s=receiveEvent(s,{...delta(3,""),kind:"task_cancelled",message:"已取消",data:undefined});
  const text=s.snapshot.tasks[0].stream_output;s=receiveEvent(s,delta(4,"迟到"));
  s=receiveEvent(s,{...delta(5,""),kind:"task_started",data:undefined});
  expect(s.snapshot.tasks[0].stream_output).toBe(text);expect(s.snapshot.tasks[0].status).toBe("cancelled");
 });
 it("重复工具ID按最近未结束执行配对，缺少开始记录显式标记",()=>{
  const start=(sequence:number):RunEvent=>({...delta(sequence,""),kind:"tool_started",data:{kind:"tool_started",call_id:"r",name:"read_file"}});
  const finish=(sequence:number,success=true):RunEvent=>({...delta(sequence,""),kind:"tool_finished",data:{kind:"tool_finished",call_id:"r",name:"read_file",success,output:"真实输出"}});
  const items=taskTools([finish(1),start(2),finish(3),start(4)],{...task,status:"cancelled"});
  expect(items.map(x=>x.status)).toEqual(["completed","completed","interrupted"]);expect(items[0].missingStart).toBe(true);
 });
 it("上下文状态保存在任务上，超过300条事件及重开快照仍保留",()=>{
  const estimate:RunEvent={...delta(1,""),kind:"context_estimate",data:{kind:"context_estimate",estimated_input_tokens:4567,context_window:128000,compression_threshold:80,source:"native_local",phase:"request"}};
  const compaction:RunEvent={...delta(2,""),kind:"context_compaction",data:{kind:"context_compaction",state:"completed"}};
  let s=receiveSnapshot(createEventState(),snapshot(0));
  s=receiveEvent(s,estimate);s=receiveEvent(s,compaction);
  for(let n=3;n<=700;n++) s=receiveEvent(s,delta(n,"x",n%2?"t":"other-task"));
  expect(s.snapshot.events).toHaveLength(300);
  expect(s.snapshot.events.some(event=>event.kind.startsWith("context_"))).toBe(false);
  expect(s.snapshot.tasks[0].context_estimate).toMatchObject({estimated_input_tokens:4567,updated_at:"now"});
  expect(s.snapshot.tasks[0].context_compaction).toEqual({state:"completed",updated_at:"now"});
  const persisted=JSON.parse(JSON.stringify({...s.snapshot,events:[]})) as Snapshot;
  const reopened=receiveSnapshot(createEventState(),persisted);
  expect(reopened.snapshot.tasks[0].context_estimate).toEqual(s.snapshot.tasks[0].context_estimate);
  expect(reopened.snapshot.tasks[0].context_compaction).toEqual(s.snapshot.tasks[0].context_compaction);
 });
 it("迟到快照重放新的上下文状态，重复和终态迟到事件不覆盖",()=>{
  const estimate={estimated_input_tokens:10,context_window:128000,compression_threshold:80,source:"native_local",phase:"request",updated_at:"before"};
  let s=receiveSnapshot(createEventState(),snapshot(1,"",[{...task,context_estimate:estimate}]));
  const newer:RunEvent={...delta(2,""),kind:"context_estimate",data:{kind:"context_estimate",estimated_input_tokens:20,context_window:128000,compression_threshold:80,source:"native_local",phase:"request"}};
  s=receiveEvent(s,newer);
  s=receiveEvent(s,{...delta(3,""),kind:"context_compaction",data:{kind:"context_compaction",state:"completed"}});
  s=receiveSnapshot(s,snapshot(1,"",[{...task,context_estimate:estimate}]));
  expect(s.snapshot.tasks[0].context_estimate?.estimated_input_tokens).toBe(20);
  expect(s.snapshot.tasks[0].context_compaction?.state).toBe("completed");
  s=receiveEvent(s,{...newer,data:{kind:"context_estimate",...estimate,estimated_input_tokens:999}});
  s=receiveEvent(s,{...delta(4,""),kind:"task_cancelled",data:undefined});
  s=receiveEvent(s,{...newer,sequence:5,data:{kind:"context_estimate",...estimate,estimated_input_tokens:888}});
  expect(s.snapshot.tasks[0].context_estimate?.estimated_input_tokens).toBe(20);
 });

});
