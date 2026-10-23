# 第二阶段协议

本文件记录第二阶段实现协议；字段名保留英文，所有产品文案与模型默认回复使用中文。文件名延续协作阶段路径。

## 文件分工

- 存储 Agent：`crates/harness-core/**`、`crates/harness-daemon/src/store.rs`。
- 编排 Agent：`crates/harness-daemon/src/lib.rs`、`provider.rs`、新增 `coordinator.rs`。
- 前端 Agent：`src/**`、`public/**`、`assets/icon.svg`。
- 主 Agent：测试脚本、文档、桌面桥接、集成验证及提交。

## 快照与命令

在现有协议上增加 `plans: Plan[]`，`TaskRun` 增加可选 `plan_id`、`node_id`、`task_kind`（`worker` 或 `plan`）。旧数据兼容。

子任务还可携带 `parent_task_id` 和 `objective`：`title` 为简短任务标题，`objective` 为完整目标及依赖实际结果。快照保持兼容，缺少 plans 的旧快照由前端按空列表处理。

Plan：

```text
id, session_id, task_id, title, source, execution_mode,
status: draft | running | reviewing | waiting_input | completed | failed | cancelled,
revision: integer, round: integer, max_rounds: 3,
created_at, updated_at, summary?, error?,
nodes: PlanNode[]
```

PlanNode：

```text
id, title, objective, depends_on: string[],
status: pending | queued | running | completed | failed | cancelled | blocked,
task_id?, result?, error?
```

- `send_message {session_id?, content}`：持久化用户消息和编排作业，立即返回收据。无需等待模型响应；未配置模型时返回中文配置提示，不虚构模型结果。
- `update_plan {plan_id, expected_revision, nodes: [{id,title,objective,depends_on}]}`：仅 draft 可编辑，乐观版本检查；节点数 1–12，唯一 ID，合法依赖，无自依赖／环路。revision 加一。
- `start_plan {plan_id, expected_revision}`：从 draft 启动，或从 waiting_input 重新请求一次 review（预算耗尽不能越过限制）。启动时创建父 TaskRun，保存 plan.task_id；draft 时 task_id 可为 null。
- `cancel_plan {plan_id}`：取消计划、排队／执行中子任务与父任务，终止后不接受迟到模型结果。
- 原有 `start_task` 等命令保留。
- 收据可新增 `plan_id`，`job_id`。

所有指令仍采用既有 command_id 幂等事务。命令处理不得等待模型网络请求。

## 主 Agent 与持久作业

Store 增加：

```rust
claim_coordinator_job(&mut self) -> Result<Option<Value>>
coordinator_context(&self, job: &Value) -> Result<Value>
complete_coordinator_job(&mut self, job_id: &str, result: Result<Value>)
    -> Result<Vec<RunEvent>>
coordinator_job_active(&self, job_id: &str) -> Result<bool>
```

作业字段：`id, kind: message|review, session_id, source, content?, plan_id?, status`。context 提供本会话近期消息、任务和目标计划，不包含密钥。

编排 Agent 独立 Tokio worker 调用以上方法，不持有 Store/settings 锁跨 await；模型请求和子任务执行不占同一个 worker。每次 claim 先推进计划状态和可运行节点。任务完成也触发计划推进。持久作业一次只提交一次结果；重启可重试未提交的主 Agent 请求，已经执行的项目任务不得盲目重放。

message 模型输出 JSON：

```json
{"action":"plan","message":"中文说明","title":"目标标题","nodes":[{"id":"step-1","title":"任务标题","objective":"完整、可独立执行的任务描述","depends_on":[]}]}
```

或 `{"action":"reply","message":"中文回答"}`。plan 生成草稿并保存，用户审阅后启动。

review 模型输出：

```json
{"action":"complete","message":"最终中文汇总"}
```

或 `{"action":"extend","message":"补充原因","nodes":[...]}`；追加节点可引用本计划已完成节点或本轮新节点。已失败节点不可作为必须成功的依赖；新节点 ID 不得与旧节点冲突。

或 `{"action":"wait_input","message":"需要用户决定的事项"}`。

仅在全部子任务完成／失败／阻塞后 review。有失败时不能接受 `complete` 并声称成功；应进入 waiting_input 或追加修复任务。round 最多 3，累计最多 12 节点，超出进入 waiting_input。无待处理节点时只创建一个 review 作业。子任务的 objective 中包含依赖结果，确保结果真实传递。

本阶段没有结构化的修复替代关系：历史失败、取消或阻塞节点始终阻止整个计划自动成功，即使追加任务完成，也保留 waiting_input。预算 round 在启动时为 1，每次追加执行或人工重新请求回顾时加一。

Demo 明确使用固定中文演示计划／回顾，不调用模型、不声称真实执行。

主智能体通过非流式 Chat Completions 请求 JSON 对象，无工具权限；上下文不超过 512 KiB、响应不超过 256 KiB，单次请求最多 120 秒。每 100ms 检查回顾作业是否仍有效，取消后丢弃挂起请求。

## 子任务、回调及恢复

父 TaskRun 标记 task_kind=plan，不被普通任务 worker claim。子 TaskRun 关联 plan_id/node_id。独立 ready 节点可排队，同项目仍通过现有写入门锁串行执行。依赖失败节点应成为 blocked，不永久悬挂。

子任务保持现有固定来源回调；计划结束通过父 TaskRun 的既有终态事务生成最终回调。取消同样必须去重。回调来源继承创建计划时的 SourceContext，后续消息／选择会话不覆盖来源。

重启恢复需要覆盖：执行中子任务失败、父任务不被误判为普通工作失败、计划继续推进至 review／waiting_input、迟到结果不会复活已取消计划。

## 前端

发送目标触发主 Agent；旧“执行任务”保留为直接运行入口。计划区展示草稿、版本、依赖、执行状态、轮次；草稿可编辑标题／任务描述／依赖，增删节点，保存后显式启动。支持取消计划。图显示真实依赖，执行时不可修改。

全部静态文案中文，用户输入、模型原始内容、协议标识和路径不做伪翻译。用中性灰和蓝色强调替换所有绿色（含边、状态、图标、背景、favicon）。保留 Codex 风格的整体布局。
