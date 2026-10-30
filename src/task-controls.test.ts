import { describe, expect, it } from "vitest";
import { canContinue, canSteer, continuationCommand, createCommandGate, inputCommand } from "./task-controls";
import { DESKTOP_SOURCE, type TaskRun } from "./types";
const task: TaskRun = { id:"task-1",session_id:"session-1",task_kind:"main",title:"原任务",status:"running",execution_mode:"native",source:DESKTOP_SOURCE,steps:[],created_at:"now",updated_at:"now" };
describe("显式任务输入目标", () => {
  it("排队始终创建新任务，补充保留指定目标", () => {
    expect(inputCommand("session-1",{mode:"queue"}," 下一条 ",[task])).toEqual({type:"run_agent",session_id:"session-1",content:"下一条"});
    expect(inputCommand("session-1",{mode:"steer",taskId:task.id}," 补充 ",[task])).toEqual({type:"steer_task",task_id:task.id,content:"补充"});
    expect(canSteer({...task,status:"queued"})).toBe(false);
    expect(canSteer({...task,execution_mode:"demo"})).toBe(false);
  });
  it("目标结束、切换会话或消失时不得改投新的运行任务", () => {
    const next = {...task,id:"task-2"};
    for(const tasks of [[{...task,status:"completed"},next],[{...task,session_id:"session-2"},next],[next]])
      expect(()=>inputCommand("session-1",{mode:"steer",taskId:task.id},"补充",tasks)).toThrow("重新选择");
    expect(canSteer({...task,task_kind:"worker"})).toBe(false);
    expect(()=>inputCommand("session-1",{mode:"queue"},"  ",[])).toThrow();
  });
  it("继续仅支持终态主任务，显式新任务且内容可选", () => {
    for(const status of ["completed","cancelled","failed"]){
      const terminal={...task,status}; expect(canContinue(terminal)).toBe(true);
      expect(continuationCommand(terminal,"")).toEqual({type:"continue_task",task_id:task.id});
      expect(continuationCommand(terminal," 修复剩余问题 ")).toEqual({type:"continue_task",task_id:task.id,content:"修复剩余问题"});
    }
    expect(()=>continuationCommand(task,"")).toThrow();
    expect(canContinue({...task,status:"failed",task_kind:"plan"})).toBe(false);
  });
  it("双击和重复快捷键不重送写入；失败释放锁但不自动重试", async () => {
    const gate=createCommandGate(); let calls=0; let finish!:()=>void;
    const first=gate.run(async()=>{calls++;await new Promise<void>(resolve=>{finish=resolve;});return "receipt";});
    expect(await gate.run(async()=>{calls++;})).toBeUndefined(); expect(calls).toBe(1);
    finish();expect(await first).toBe("receipt");
    await expect(gate.run(async()=>{calls++;throw new Error("transport unknown");})).rejects.toThrow();
    expect(calls).toBe(2);expect(await gate.run(async()=>"next explicit command")).toBe("next explicit command");
  });
});
