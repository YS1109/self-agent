import {describe,it,expect} from "vitest";
import {startupEvent} from "./use-daemon-startup";
describe("bounded startup observations",()=>{
  it("distinguishes starting, actionable timeout and readiness without granting any operation",()=>{
    expect(startupEvent({status:"starting",message:"等待协议就绪"})).toEqual({phase:"starting",message:"等待协议就绪"});
    expect(startupEvent({status:"startup_error",message:"未自动重启"})?.phase).toBe("error");
    expect(startupEvent({status:"connected"})).toEqual({phase:"idle",message:""});
    for(const value of [null,[],{status:"starting",message:1},{status:"other",message:"secret"},{status:"starting",message:"中".repeat(400)}])expect(startupEvent(value)).toBeNull();
  });
});
