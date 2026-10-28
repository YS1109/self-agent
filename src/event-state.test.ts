import { describe, expect, it } from "vitest";
import { createEventState, receiveEvent, receiveSnapshot, taskTools } from "./event-state";
import { EMPTY_SNAPSHOT, DESKTOP_SOURCE, type RunEvent, type Snapshot, type TaskRun } from "./types";
const task: TaskRun = { id:"t", session_id:"s", title:"目标",status:"running",execution_mode:"native",created_at:"now",updated_at:"now",source:DESKTOP_SOURCE,steps:[],task_kind:"main" };
const snapshot = (cursor: number, text="", tasks=[task]): Snapshot => ({...EMPTY_SNAPSHOT,event_cursor:cursor,tasks:tasks.map(t=>({...t,stream_output:text}))});
const delta = (sequence:number,text:string,task_id="t"):RunEvent => ({id:`e${sequence}`,sequence,task_id,kind:"assistant_delta",message:"回复增量",created_at:"now",data:{kind:"assistant_delta",text}});

describe("持久事件与快照合并",()=>{
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
});
