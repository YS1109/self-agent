# 协议 v8：托管 MCP 与固定版本 Skills（0.8 开发切片）

本文叠加在 [v7 执行控制](protocol-v7.md) 和 [v6 流式事件](protocol-v6.md) 上。这里的注册、发现、启用、授权和实际调用是不同状态；配置成功不表示工具执行成功。命名模型服务与系统凭据生命周期属于下一独立切片，当前 MCP `credential_refs` 必须为空，不允许在程序参数中填写凭据。

## 本机所有者管理入口

所有入口沿用后台本机 bearer 身份验证。管理入口不是模型工具，也不是远程通讯渠道的通用代理接口；后续网关不得把后台令牌或此管理权限交给远程消息。

- `GET /api/v1/capabilities`：公开 MCP/Skills 目录，仅含配置元数据、修订、发现状态、摘要、来源和大小；不包含 Skill 正文、解析后的凭据或工具结果。
- `POST /api/v1/capabilities/commands`：最多 256 KiB，封包 `{id, command}`。`id` 为命令 UUID；同 ID、同封包返回原回执，不重新启动进程或重新读取导入路径；同 ID、不同封包拒绝。
- 回执 `{command_id,status,capability_id,revision}`；`status` 为 `completed` 或 `checking`。未知 HTTP 结果应以同 ID 查询/重试，不自动创建新动作。

管理命令：

| type | 参数与含义 |
| --- | --- |
| `save_mcp_server` | 可省 `id`，编辑需要 `expected_revision`；`definition` 含 `name,program,args,protocol,enabled,credential_refs`。绝对程序路径、JSON argv，不经 shell。新建默认禁用，保存不启动，不隐式启用。 |
| `probe_mcp_server` | `id,expected_revision,confirm_start:true`。明确确认后在独立后台数据目录启动可信本机程序，获取工具目录；不执行 `tools/call`。 |
| `save_skill` | 可省 `id`，编辑需要 `expected_revision`；`name,content`。显式保存正文，不扫描目录。 |
| `import_skill` | 同编辑条件；`name,source_path`。只读取明确指定的绝对 `SKILL.md`；逐级拒绝符号链接/重解析，普通 UTF-8 文件，最多 64 KiB。 |
| `set_capability_enabled` | `kind: mcp|skill,id,expected_revision,enabled`。启用不是任务授权。 |
| `delete_capability` | `kind,id,expected_revision`。保留 UUID 墓碑，不把旧 ID 重新分配给其他能力。 |

目录上限为 32 个 MCP 服务、64 个 Skills。名称最多 80 字符；argv 最多 64 项、每项 8192 字节、合计 64 KiB。已知凭据不能出现在名称、参数、目录元数据或正文中；检查原始字符串和 JSON 键，不能先转义再检查。

每次修改/显式发现递增修订。发现状态为 `configured/checking/ready/unavailable`，`ready` 仅表示通过了有界目录发现。后台单次领取持久发现意图；重启中的发现变为不可用，不自行重跑。发现最多并发 2 个，超额请求保留不可用结果，需重新显式确认。旧完成回调不能覆盖编辑或删除后的记录。

## 任务快照与权限

`TaskControls` 新增：

```json
{
  "permissions": {"managed_tools":"deny"},
  "budget": {"max_external_calls":32},
  "capabilities": {"mcp_servers":[],"skills":[]}
}
```

- MCP 权限默认拒绝，独立于文件/命令权限；全局、角色和任务选择取交集，每类最多 8 个。任务 MCP 目录合计最多 128 个工具，Skills 正文合计最多 128 KiB。
- MCP 是所有者明确选定的**无文件或网络沙箱的本机程序**。服务的 `readOnlyHint` 等 annotations 仅为不可信提示，不能当作只读保证。禁止读取/写入时也须禁止 MCP；写入需询问时 MCP 也须询问或禁止。
- `managed_tools:ask` 在启动前产生一次具体操作审批：服务 UUID/修订、程序与 argv、工具名称、脱敏有界参数预览，以及全部原始参数的摘要。预览明确标记截断/隐藏；批准不是泛化服务信任。
- 服务准备完成、发送 `tools/call` 前再次核验修订、撤销和权限。准备期间从允许收紧为询问，不能沿用旧允许结论；停止该操作，建立新任务重新授权。
- 外部调用预算默认 32、范围 0–128；在启动前消耗一次，包含该次启动/发现/调用尝试，失败不退还。所有外部工具按可能修改计入验收版本，不能依赖旧版本的测试成功。
- MCP 任务私有快照保存确切配置/工具目录；公开 `managed_capabilities` 只有 `{id,revision,name,digest}`。任意 MCP 配置/目录修订、禁用、删除或全局移除会使旧任务不能继续使用；不会静默换到新进程配置。
- Skills 是明确选定的固定正文，不自动加载旁边脚本、资源或 `allowed-tools` 权限。正文作为带来源/摘要的低权限任务说明注入 Native/Codex；编辑仅影响新任务，禁用/删除撤销旧授权，禁用后再启用也不复活旧任务。
- Native 摘要和 Codex 续接指纹包含执行控制及固定能力引用。应用管理的 MCP 目前仅供 Native；Codex 选中 MCP 会在启动前拒绝，不修改用户 Codex 配置，也不声称控制其继承的外部 MCP。

## MCP 传输与结果

显式选择 `modern2026`（MCP `2026-07-28`）或 `legacy2025`（`2025-11-25`），不试探回退。Modern 采用 `server/discover` 和逐请求协议元数据；Legacy 采用 `initialize`、`notifications/initialized`。本切片只支持 stdio 的工具发现/调用，不自动提供 HTTP、sampling、elicitation、后台 task、MRTR 后续交互、资源读取或外部 URL 跳转。

单行 1 MiB；单操作累计 4 MiB（包含预留 stderr 上限）；stderr 生命周期最多 64 KiB，丢弃而不记录原文。目录最多 8 页/128 工具，schema 32 KiB，参数 64 KiB，通知最多 256 条。启动/发现默认 10 秒，调用默认 30 秒，均不能越过任务剩余时长。stdin 背压、静默进程和部分 UTF-8 读取均受取消/截止约束。

参数 schema 使用有界严格子集：单一 `type`、`properties/required/additionalProperties/items`、`enum/const`、数值/长度边界及无效能授予作用的说明字段。外部 `$ref`、正则、组合 schema 等不受支持字段会拒绝目录，而非静默忽略；递归深度 16、schema 节点 512。数字限制为可精确表示的安全范围（绝对值不超过 `2^53-1`），不通过浮点舍入接受越界整数。

Native 只看到不透明 `mcp_<server UUID>_<tool hash>` 别名，不能通过模型参数选择可执行程序或环境。调用时重新发现并比较规范排序后的完整目录；变化会在工具调用前拒绝。工具结果只公开脱敏、有界文本/结构化数据；先递归处理原始 JSON 字符串键和值，再序列化/截断；图片、资源和链接不自动读取。输出在持久化/发送模型前统一计费一次。

`task.capability_calls` 保存 `{id,task_id,server_id,revision,tool,digest,status,reason,created_at,updated_at}`，最多 128 条，不保存原始参数或结果。`sending/completed/tool_error/not_sent/unknown` 描述实际传输；`not_sent` 只指工具请求未发送，不证明进程启动没有副作用。发送后的 EOF、取消、超时、不支持的续交互等为 `unknown`：停止依赖任务，不自动重放；重启将未结束调用标为未知。

## 进程清理与项目隔离

Unix 使用独立进程组、保持的目录 fd 与 `fchdir`，不在校验后重新按路径打开 cwd。进程组结束不等于恶意进程不能主动脱离组；这不是沙箱。

Windows 后端采用 suspended `CreateProcessW`、Job `KILL_ON_JOB_CLOSE`、成功绑定后才恢复主线程；显式继承句柄清单、清空环境、只接受固定本地磁盘、锁定根与可执行文件各级路径且拒绝重解析/映射网络盘。Job 清理确认须同时观察根进程退出和 Job 活跃进程数为零。Windows 运行验证结果须以对应提交的 CI 为准，不能由 Linux 交叉编译替代。

已启动操作必须等待清理结果再返回。若清理无法确认，持久化 `execution_cleanup:unknown` 并隔离同项目后续执行；重启不能清除隔离。普通未知工具结果但进程清理已确认时，仅停止当前任务，不自动扩大为永久项目隔离。

本机所有者可在操作系统中确认相关进程全部停止并检查项目改动后，显式调用：

`POST /api/v1/tasks/{task_id}/cleanup-resolution`

```json
{"expected_updated_at":"快照中的 execution_cleanup_updated_at","confirm_processes_stopped":true}
```

只接受终态且当前清理未知的任务，按时间边界条件更新；相同确认幂等，过时确认拒绝。解除隔离仅是所有者确认记录，**不改写原任务失败/取消、未知调用结果、文件改动或验收结论**。该入口不向模型和远程通讯授权。

## 参考与验证边界

- [MCP 2026 工具协议](https://modelcontextprotocol.io/specification/2026-07-28/server/tools)
- [MCP 2026 发现](https://modelcontextprotocol.io/specification/2026-07-28/server/discover)
- [MCP 2025 生命周期](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)
- [Agent Skills 格式](https://agentskills.io/specification)：本切片提供显式正文导入/固定使用，不宣称完整自动发现或执行资源支持。

当前验证只使用本地模型和 MCP 夹具，不访问真实模型、外部服务或系统凭据。界面自动化/真实 OS vault 的未验证部分保持明确记录；最终门禁及精确远端 SHA 见验收记录。
