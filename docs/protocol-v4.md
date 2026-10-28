# 第五阶段协议

版本 0.5.0，在第四阶段协议上新增异步计划调整建议和可编辑 DAG 画布。所有入口仍使用已有命令信封、会话路由与固定来源回调。

## 自然语言调整

```json
{
  "type": "request_plan_change",
  "plan_id": "plan-id",
  "expected_revision": 7,
  "content": "先补回归测试，再继续界面修改"
}
```

接收时校验计划版本与可修改状态，固化当前计划、角色和输入来源，持久化建议与主 Agent 作业后立即返回凭证。凭证包含 `session_id`、`plan_id`、`change_id` 和 `job_id`。显式计划决定目标会话，入口当前选中的其他会话不会改变路由。

主 Agent 处理 `kind=change`，输出 `action=change`、中文 `message` 和完整 `nodes`。后台在请求时的计划快照上校验输出，保存建议，绝不自动替换正在执行的计划。建议生成过程中，工作节点、快照查询和结构化控制继续可用。

快照增加 `plan_changes`：

```json
{
  "id": "change-id",
  "plan_id": "plan-id",
  "session_id": "session-id",
  "base_revision": 7,
  "source": {"channel_id": "desktop", "actor_id": "owner", "conversation_id": "main"},
  "content": "补充要求",
  "status": "ready",
  "nodes": [],
  "message": "修改摘要",
  "created_at": "RFC3339",
  "updated_at": "RFC3339"
}
```

状态为 `queued / running / ready / applied / failed / discarded`，失败时增加 `error`。示例空 `nodes` 仅表示字段类型；可应用建议仍须包含 1–12 个有效节点。

```json
{"type": "apply_plan_change", "change_id": "change-id", "expected_revision": 7}
{"type": "discard_plan_change", "change_id": "change-id"}
```

应用必须满足 `expected_revision == 当前计划 revision == base_revision`。实际写入复用 `update_plan / patch_plan` 的全部依赖、历史锁定、修复验收、累计预算与事务校验。计划版本变化后建议保留，应用原子拒绝；客户端保留原文，允许基于当前版本重新生成。

取消计划、丢弃建议后，迟到的模型结果不能恢复计划或应用修改。后台重启可重新生成未完成建议，保留已经 ready 的建议，不自动应用。输入建议的来源保存在 `PlanChange.source`；子任务和最终计划回调继续使用原计划来源。

## 画布结构与布局

画布和表单共享同一结构草稿，仍通过 `update_plan / patch_plan` 保存，不能修改已开始或终态节点的定义。拉线、删除依赖与表单勾选依赖具有相同语义。结构限制为 DAG，尚无条件分支、显式循环或人工确认节点。

布局与执行结构分开版本化：

```json
{
  "type": "set_plan_layout",
  "plan_id": "plan-id",
  "expected_layout_revision": 2,
  "layout": {
    "node-a": {"x": 40, "y": 80},
    "node-b": {"x": 320, "y": 240}
  }
}
```

`Plan.layout` 默认为 `{}`，`Plan.layout_revision` 默认为 `0`。布局只改变位置与布局版本，不改变执行 `revision`、不触发派发、不使自然语言建议过期。可布局已经结束的计划，但不因此开放历史定义编辑。

布局版本过期时原子拒绝。坐标必须有限且位于 ±100000，最多 12 个位置，节点必须存在。结构删除节点时清理对应布局并推进布局版本。新建节点需要先保存结构，再保存布局。

## 验证入口

0.5.2 配置扩展沿用本机认证：

- `GET /api/v1/settings` 返回有效来源的公开地址、模型、`provider_source`、`configured`、`api_key_configured` 与 `configuration_error`；不返回凭据或请求头。`configured` 只表示配置可解析，不代表已请求模型。
- `PUT /api/v1/settings` 支持 `provider_source: "manual" | "local_codex"`，缺失时保留原来源，旧记录默认为 manual。local_codex 每次请求读取本机顶层自定义服务，保存可选模型覆盖；凭据与源地址不复制到配置记录。
- `GET /api/v1/provider/local-codex` 只读来源公开元信息，不保存、不请求模型。
- `POST /api/v1/settings/check` 接受与设置相同的草稿字段，发出一次无工具的 JSON 模型请求。成功返回 `{ok: true, message}`，失败返回中文错误；不保存、不建任务，等待模型时不持有设置锁。最长 30 秒、响应最多 256 KiB。
- `POST /api/v1/models` 可显式指定 `provider_source`；来源为 local_codex 时使用源地址及鉴权，不采用草稿中的地址或密钥。manual 仅在同一来源和相同地址复用内存密钥。

`python scripts/plan_change_smoke.py` 使用确定性模型响应和真实 Native 工具，覆盖独立布局版本、建议异步接收与幂等、跨入口显式路由、确认后应用、过期拒绝、重启，以及失败修复／命令验收／原来源回调。默认三平台 CI 执行该脚本。

显式云端验收：

```sh
HARNESS_BASE_URL=... HARNESS_MODEL=... HARNESS_API_KEY=... \
  python scripts/plan_change_smoke.py --live --evidence .local/v05-live-evidence.json
```

`--live` 使用真实主模型制定计划、生成修改建议、回顾失败与安排修复；子任务使用已配置的真实 Codex。测试仅在隔离临时项目中执行，保留原始测试文件，脱敏证据只记录节点状态、命令、退出码与回调数量。默认 CI 不使用云端额度。
