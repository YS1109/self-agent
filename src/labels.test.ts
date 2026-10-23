import { describe, expect, it } from "vitest";
import { capabilityLabel, eventLabel, localizeSystemText, STATUS } from "./labels";
describe("中文静态文案", () => {
  it("只翻译已知旧版系统模板，保留模型原文与协议标识", () => {
    expect(localizeSystemText("Understand the objective")).toBe("理解目标");
    expect(localizeSystemText("Received. 2 task(s) active in this session. Use Run task to dispatch coding work; you can keep managing sessions while workers execute.")).toContain("2 个活动任务");
    expect(localizeSystemText("Native worker: model turn 3 of 10")).toBe("原生执行器：模型轮次 3 / 10");
    const modelResult = "Read README.md and update API docs. Native worker: example";
    expect(localizeSystemText(modelResult)).toBe(modelResult);
  });
  it("能力名称与状态事件使用中文，原始未知能力保留来源信息", () => {
    expect(capabilityLabel({ id: "main", name: "Main agent", description: "Old English", enabled: true, kind: "agent" }).name).toBe("主智能体");
    expect(eventLabel("callback_delivered")).toBe("回调已送达");
    expect(STATUS.paused).toBe("已暂停");
    expect(eventLabel("plan_paused")).toBe("计划已暂停");
    expect(eventLabel("plan_resumed")).toBe("计划已恢复");
    expect(eventLabel("external_event")).toBe("执行事件");
    expect(capabilityLabel({ id: "custom", name: "Custom provider", description: "User text", enabled: false, kind: "tool" }).description).toBe("User text");
  });
});
