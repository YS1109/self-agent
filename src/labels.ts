import type { Capability } from "./types";

export const STATUS: Record<string, string> = {
  draft: "待审阅",
  queued: "排队中",
  running: "执行中",
  reviewing: "主智能体回顾中",
  waiting_input: "需要你介入",
  completed: "已完成",
  failed: "失败",
  cancelled: "已取消",
  pending: "待处理",
  blocked: "依赖受阻",
  sending: "投递中",
  sent: "已送达",
};
const EVENT_LABELS: Record<string, string> = {
  session_created: "会话已创建",
  session_selected: "会话已切换",
  message_received: "指令已接收",
  task_created: "任务已创建",
  task_queued: "任务已排队",
  task_started: "任务已开始",
  task_progress: "任务进展",
  task_completed: "任务已完成",
  task_failed: "任务失败",
  task_cancelled: "任务已取消",
  task_recovered: "任务已恢复",
  delivery_retried: "回传已重试",
  delivery_updated: "回传状态更新",
  callback_sent: "回调已送达",
  callback_delivered: "回调已送达",
  callback_failed: "回调失败",
  callback_retry: "回调重试",
  coordinator_queued: "主智能体已接收",
  coordinator_started: "主智能体处理中",
  coordinator_completed: "主智能体处理完成",
  coordinator_failed: "主智能体处理失败",
  coordinator_replied: "主智能体已回复",
  coordinator_needs_configuration: "需要配置模型",
  plan_created: "计划已生成",
  plan_updated: "计划已保存",
  plan_started: "计划已启动",
  plan_progress: "计划进展",
  plan_reviewing: "计划回顾中",
  plan_extended: "计划已追加任务",
  plan_waiting_input: "计划需要介入",
  plan_completed: "计划已完成",
  plan_failed: "计划失败",
  plan_cancelled: "计划已取消",
};
export const eventLabel = (kind: string) => EVENT_LABELS[kind] ?? "执行事件";
export const kindLabel = (kind: string) =>
  ({ executor: "执行器", agent: "智能体", tool: "工具", channel: "通讯入口", skill: "技能", provider: "模型服务", integration: "集成", mcp: "MCP 工具" })[kind] ?? "能力";

const CAPABILITIES: Record<string, { name: string; description: string }> = {
  native: { name: "原生执行器", description: "由本机后台调用模型并执行项目工具循环。" },
  native_executor: { name: "原生执行器", description: "由本机后台调用模型并执行项目工具循环。" },
  main: { name: "主智能体", description: "接收目标、生成计划，并在子任务结束后回顾与继续编排。" },
  workspace: { name: "项目文件", description: "在选定项目目录内读取、列举、搜索和写入文件。" },
  shell: { name: "项目命令", description: "可信本地命令执行。通过 HARNESS_ALLOW_COMMANDS=1 开启，不具备操作系统沙箱隔离。" },
  callback: { name: "本地回调", description: "将任务结果持久化回传到桌面和远程模拟器，无需再次调用模型。" },
  demo: { name: "演示执行器", description: "使用固定演示流程验证编排和回传，不修改项目。" },
  codex: { name: "Codex 执行器", description: "调用本机 Codex CLI 执行子任务，使用本机登录状态与配置；可在角色设置中选择。" },
  coordinator: { name: "主智能体", description: "接收目标、生成计划，并在子任务结束后回顾与继续编排。" },
  main_agent: { name: "主智能体", description: "接收目标、生成计划，并在子任务结束后回顾与继续编排。" },
  mcp: { name: "MCP 工具", description: "外部工具协议接入，尚未启用。" },
  skills: { name: "技能目录", description: "可复用的智能体技能，尚未接入。" },
  simulator: { name: "远程指令模拟器", description: "在本地验证远程会话选择、指令派发和回调。" },
};
export function capabilityLabel(capability: Capability) {
  return CAPABILITIES[capability.id] ?? {
    name: capability.name,
    description: capability.description,
  };
}

const HISTORICAL_SYSTEM_TEXT: Record<string, string> = {
  "Understand the objective": "理解目标",
  "Execute with workspace tools": "使用项目工具执行",
  "Report evidence and limitations": "报告结果、证据与限制",
  "Task dispatched to the background worker. This session remains available.": "任务已派发到后台执行，你可以继续在此会话中发送指令。",
  "Demo: staging a synthetic planning step": "演示：模拟规划步骤",
  "Demo: staging a synthetic execution step": "演示：模拟执行步骤",
  "Demo: staging a synthetic verification step": "演示：模拟验证步骤",
  "Demo completed. This was synthetic work; no project files were changed and no real checks were run.": "演示已完成。本次为模拟执行，没有修改项目文件，也没有运行真实检查。",
  "Session created": "会话已创建",
  "Session selected": "会话已切换",
  "Callback retry requested": "已请求重试回调",
  "Session selection updated for this source": "此入口已切换会话",
  "Source session selected": "此入口已切换会话",
  "Main agent acknowledged your message": "主智能体已接收指令",
  "Background task accepted": "后台任务已接收",
  "Worker claimed task": "执行智能体已领取任务",
  "Result delivered to the durable local channel sink": "结果已持久化投递到本地回传入口",
  "Callback adapter unavailable": "回调适配器尚未接入",
  "Channel adapter not configured": "通讯入口适配器尚未配置",
  "Cancelled by user. Existing workspace changes are retained.": "用户已取消任务，现有项目修改已保留。",
  "Daemon restarted during execution; inspect workspace changes before retrying": "后台在执行时重启，请先检查项目修改，再决定是否重试。",
  "Daemon restarted during execution. Inspect project changes before starting a new task; execution was not replayed.": "后台在执行时重启，请先检查项目修改，再决定是否启动新任务；原任务没有自动重放。",
};
// Translate only known legacy application templates, never arbitrary model or user prose.
export function localizeSystemText(text: string): string {
  if (HISTORICAL_SYSTEM_TEXT[text]) return HISTORICAL_SYSTEM_TEXT[text];
  const receipt = /^Received\. (\d+) task\(s\) active in this session\. Use Run task to dispatch coding work; you can keep managing sessions while workers execute\.$/.exec(text);
  if (receipt) return `指令已接收，当前会话有 ${receipt[1]} 个活动任务。你可以继续管理会话，后台任务会持续执行。`;
  const turn = /^Native worker: model turn (\d+) of (\d+)$/.exec(text);
  if (turn) return `原生执行器：模型轮次 ${turn[1]} / ${turn[2]}`;
  const tool = /^Native worker: (list_files|read_file|search_files|write_file|run_command)$/.exec(text);
  if (tool) return `原生执行器：${({ list_files: "列出文件", read_file: "读取文件", search_files: "搜索文件", write_file: "写入文件", run_command: "运行命令" })[tool[1]]}`;
  return text;
}
