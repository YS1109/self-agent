# 第七阶段协议：持久事件与 Native 流式首切片

日期：2026-10-28。状态：0.7 开发分支首切片，尚未发布完整 0.7；应用版本号暂仍为 0.6.0。本文描述本分支已实现接口，未列能力继续沿用 [0.6 协议](protocol-v5.md)。

本切片贯通共享 Rust 后台、Native 直接任务、HTTP 事件恢复和 CLI 跟随。Codex 通知映射、桌面事件桥、Codex thread 续接及 Native 跨任务摘要复用尚未实现；不能据本文宣称 0.7 P0 已全部完成。

## 事件与事实来源

`GET /api/v1/events` 沿用本机 Bearer 认证，只监听现有 loopback 服务。客户端不得把后台令牌交给浏览器前端。SSE 格式：

```text
id: 42
event: run_event
data: {"id":"event-uuid","sequence":42,"session_id":"session-id","task_id":"task-id","kind":"assistant_delta","message":"回复增量","created_at":"...","data":{"kind":"assistant_delta","text":"你好"}}
```

- `id` 仍是事件 UUID；新增 `sequence` 为数据库全局递增提交顺序，SSE `id` 改为同一十进制序号，不再使用 UUID。序号可以有空隙，不要求连续；客户端按已成功应用的序号去重，不能按文本去重。
- SQLite 是唯一事实来源；回放与实时读取走同一个有序日志。广播仅唤醒订阅者。慢消费者不会阻塞任务提交；广播溢出不会丢掉持久事件。
- 旧事件原子迁移，保留 UUID、正文和原插入顺序。事件序号在删除旧日志、重启后不复用。
- `task_completed`、`task_failed`、`task_cancelled` 延用现有终态事件。任务、最终结果、终态事件与回调仍同事务提交；迟到执行事件和进度在终态后被拒绝，终态不会重复提交。

新增事件正文 `data` 为带 `kind` 的联合类型：

| kind | 字段 | 含义 |
| --- | --- | --- |
| `assistant_delta` | `text` | 执行器实际提供的回复增量，不是隐藏推理 |
| `tool_started` | `call_id`, `name` | 完整工具参数已验证，准备执行 |
| `tool_finished` | `call_id`, `name`, `success`, `output` | 实际工具结果，输出有界并脱敏 |
| `usage` | `input_tokens`, `output_tokens` | 上游明确提供的用量，可为 null；不伪装成估算 |
| `context_compaction` | `state` | 本轮实际压缩状态；不表示摘要已支持跨任务复用 |

本切片的命令输出随 `tool_finished.output` 和已有命令证据交付，不提供逐字节命令 stdout/stderr 流。工具开始事件不代表工具已成功；失败或取消时，仍应以任务终态与实际证据为准。

## 游标、快照与断线恢复

- 无游标订阅默认从请求时的最新提交位置开始，只看后续事件；需要无缝恢复时先获取快照，再订阅该快照游标。
- `GET /api/v1/events?after=42` 或请求头 `Last-Event-ID: 42`，补发严格大于 42 的事件，再跟随新增事件。两者同时提供必须相等。`after=0` 只在起始历史尚未清理时从头补发；清理后同样要求恢复快照，不会静默跳过已丢弃历史。
- 负数、非法文本、超范围、未来游标、已经过期的游标返回 HTTP 409：`{"code":"snapshot_required","error":"..."}`。旧 UUID 游标同样要求刷新快照。
- 连接过程中若旧记录已超出保留窗口，发送 `event: resync`、`data: snapshot_required` 并结束连接。客户端重新获取快照后替换已有视图，再从新游标订阅，不能把快照文本当新增片段重复拼接。
- `GET /api/v1/snapshot` 新增 `event_cursor`，与其任务数据在同一锁定读取内一致。任务新增 `stream_output`（已提交回复累计前缀）及 `stream_output_truncated`，旧任务可没有这些字段。最终 `result`/`error` 始终是终态依据，流式正文可包含工具前的中间回复。
- 所有客户端即使筛选某任务，也应推进已看到的全局游标，避免被其他任务事件困住。重复传输允许，重复应用不允许。

输出边界：每批回放最多 128 条；事件日志保留最近最多 20,000 个序号，新事件提交时清理过期记录，历史回放不是永久审计归档。快照事件列表最多 300 条；任务回复快照上限 256 KiB，并标记截断。结构化执行事件正文上限 32 KiB，工具结果正文上限 12 KiB（超过时标记截断）；原始任务结果和命令证据遵循原有边界。超过保留窗口必须恢复快照，不承诺补发已经清理的历史片段。

## Native 流式与兼容

直接 Native 主任务和 Native 子任务的工具循环发送 Chat Completions `stream:true` 和 `stream_options.include_usage:true`，按 SSE 字节流解析。中文可跨网络字节、JSON/工具参数可跨任意分片；只有完整回合、有效 `finish_reason` 和 `[DONE]` 都取得后，才允许执行已完整验证的工具调用批次。

流式错误、截断、无效 JSON、输出预算终止、残缺参数或重复工具 ID 均失败，不自动重试模型请求，不重放已执行工具。取消会关闭在途请求，后续输出不得追加成功终态。增量文本与工具输出在进入持久事件前隐藏当前凭据，跨文本分片及工具轮次拆分的凭据都经过任务生命周期脱敏，不因累计拼接泄漏。

兼容策略：同一请求若返回 `application/json`，读取该非流式响应并明确记录兼容进度；不会偷偷重发 `stream:false`。拒绝流式请求的服务会得到可解释错误，需用户选择兼容服务。远程规划、回顾和实际摘要请求仍使用原有非流式 JSON；本切片不新增模型服务协议。

单次模型流上限 2 MiB、单个 SSE 帧 512 KiB、单轮回复 128 KiB、单任务累计回复 256 KiB、执行事件最多 8192 条。回复事件以有界分片发布；真实服务用量仅在服务提供时出现，未提供保持未知。

## CLI 跟随

原有命令默认仍输出单个 JSON，`--wait` 的超时、失败、取消退出码不变。新增显式跟随：

```sh
self-agent-cli --follow --wait 600 run SESSION_ID "读取项目并说明结果"
self-agent-cli --wait 600 follow TASK_ID
```

`--follow` 在提交任务前取得游标，POST 只发送一次；跟随重连仅重试读取，不重复任务指令。`follow TASK_ID` 先输出已有任务快照，再消费新事件。默认跟随时限 3600 秒，可用 `--wait` 指定 1–86400 秒。超时不取消后台任务。

输出改为逐行 JSON（NDJSON），每行立即刷新：

```json
{"type":"receipt","receipt":{"task_id":"..."}}
{"type":"event","event":{"sequence":42,"kind":"assistant_delta","data":{"kind":"assistant_delta","text":"你好"}}}
{"type":"task_snapshot","task":{"id":"...","status":"completed","result":"你好"},"event_cursor":43,"reset":false}
```

`reset:true` 表示附加或恢复时的权威快照，消费方应替换状态；`reset:false` 为已观察终态后的最终快照。只输出目标任务事件，但记住其他任务的全局游标。daemon 重启时重新读取私有元数据中的本机地址和令牌。成功退出 0，失败、取消、输出失败或超时非零；不输出凭据。

## 验证与剩余切片

默认验证只使用本地 HTTP / stdio 夹具，不调用真实模型。新增 `scripts/streaming_smoke.py` 并加入三平台 CI；覆盖中文、分片工具、实时增量、游标回放、取消、错误无副作用、凭据脱敏、CLI、重启和既有文件保留。单元测试覆盖迁移、事务回滚、保留窗口和重复终态。

后续依次完成：Codex/桌面统一事件消费 → 执行器会话关联及恢复语义 → Codex thread 续接与 Native 摘要复用 → 0.7 P1。真实模型、桌面原生 GUI、跨平台结果必须分别记录，不能用本机夹具结果替代。
