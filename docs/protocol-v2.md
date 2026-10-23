# 第三阶段协议

在第二阶段指令与计划协议上兼容扩展，不改变 `native | demo` 执行模式。执行器选择属于角色，演示模式仍明确模拟，不调用 Codex。

## 角色配置

`GET /api/v1/settings` 和现有 PUT 增加：

```text
agents: [{id, name, instructions, executor: native|codex, model}]
default_agent_id: string
codex_binary: string
```

1–16 个角色，唯一 ASCII ID（字母、数字、下划线、连字符，1–64 字节），合法默认角色。名称 1–200 字节、指令最多 32000 字节、模型最多 200 字节。不支持的执行器会拒绝。配置原子校验后持久化，API Key 仍不落盘。

Codex 路径为命令名或单个文件路径，不解析 shell 参数。设置默认值优先 `HARNESS_CODEX_BINARY`，其次 PATH 和常用安装目录。原生模型留空继承主 Agent 模型，Codex 模型留空继承本机 CLI 配置。

`GET /api/v1/codex/status` 需本机令牌，返回 `{available, version, message}`。检测运行已保存路径的 `--version`，时限 5 秒、输出 128 字节，不验证登录及模型访问。

## 分配与快照

- `start_task` 可带 `agent_id`，否则使用当前默认角色。
- 编排作业、Plan 增加 `agents`、`default_agent_id`；发送目标时保存角色快照。
- PlanNode 增加 `agent_id`；草稿更新、启动、模型规划和自动追加按计划快照校验，未知角色拒绝。
- TaskRun 增加 `agent`，为完整角色快照。依赖延迟派发、重启和取消不读取新角色配置。
- 主 Agent 上下文提供角色列表及默认角色；JSON 节点可以指定 `agent_id`，省略时使用默认角色。
- 旧设置、旧计划、旧编排作业和旧任务兼容原始 `coder/native` 角色，不用新设置改写历史。

角色指令会与完整任务目标一起交给执行器。主 Agent 仍只通过兼容模型生成计划／回顾，没有工具权限。Codex 和原生执行器共享项目写入门锁，管理指令不等待执行器。

## Codex 执行器

本机 CLI 已核验版本为 0.163.0。以 `codex app-server --listen stdio://` 启动独立进程，JSONL 协议顺序：

```text
initialize → initialized → thread/start → turn/start
                     item/started、item/completed 等进度
                     turn/completed → 终态保存和固定来源回调
取消 → turn/interrupt → 有界等待 → 回收进程树
```

每任务创建 ephemeral thread。显式 `approvalPolicy=never`、`workspace-write`、项目根写入、禁命令网络、禁额外临时目录，并核验 thread/start 返回权限。未知交互和额外授权明确拒绝后终止。原始 stderr、RPC 错误和协议内容不作为进度保存；进度为中文摘要。

仅 `turn/completed` 成功且存在最终非空 Agent 消息才完成；失败、EOF、坏 JSON、超预算和取消不能伪报成功。只接收本 thread/turn 的消息。每 100ms 检查取消，15 分钟总时限，256 KiB 单行、8 MiB 总协议输出、20000 行、128 KiB 输入和最终结果。

继承本机 CLI 登录和配置，不转交主 Agent 的 API Key。不会在当前阶段导入／恢复 Codex 历史会话。Codex 已配置工具可能运行；第三方远端工具行为不受本机文件沙箱保证。

协议参考：[官方 app-server 文档](https://developers.openai.com/codex/app-server/)；执行字段同时通过本机 CLI 生成 schema 验证。
