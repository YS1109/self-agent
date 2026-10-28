# 第六阶段协议：主智能体直接执行与无头入口

版本：0.6.0，2026-10-28。

日常桌面使用「用户目标 → 主智能体工具循环 → 真实结果」的方式。主智能体不再默认先创建计划草稿。远程入口继续支持生成计划、子智能体编排、会话选择、结构化管理与完成回调。

## 直接主智能体

`POST /api/v1/commands` 沿用现有认证、来源和幂等收据：

```json
{
  "command_id": "caller-generated-unique-id",
  "source": {
    "channel_id": "cli",
    "actor_id": "local",
    "conversation_id": "script-1"
  },
  "command": {
    "type": "run_agent",
    "session_id": "session-id",
    "content": "检查项目并完成修改"
  }
}
```

收据立即返回 `session_id`、`task_id`。任务以 `task_kind: "main"` 持久化，进入后台执行；查询、切换会话、取消等管理指令不等待模型。主任务保存实际模型、执行器及有效上下文策略，不套用默认子智能体职责。相同会话的主任务依次运行，启动时带入此前对话与已完成结果，不带入后续排队输入。

原有 `send_message` 继续用于远程编排或讨论，`start_task` 继续使用子智能体角色预设。旧计划、修改建议、画布、暂停恢复、验收与回调协议保持兼容。

## 模型服务与模型策略

`GET/PUT /api/v1/settings` 增加：

```json
{
  "main_executor": "native",
  "models_endpoint": "/catalog?api-version=example",
  "model_contexts": {
    "example-model": {
      "context_window": 128000,
      "compression_threshold": 80
    }
  }
}
```

- `base_url` 是 Chat Completions 服务根 Endpoint；推理请求继续使用其 `/chat/completions`。
- `models_endpoint` 为空时使用 `base_url/models`。可填相对路径、根路径或同源完整 URL，支持查询参数；拒绝跨源、URL 内凭据、片段和重定向，避免模型列表请求转发凭据到另一服务。
- `POST /api/v1/models` 的草稿也支持 `models_endpoint`，发现不保存设置，不改变已有模型选择。换服务不沿用旧服务密钥。
- `main_executor` 为 `native | codex`，默认 `native`；Codex 直接执行不要求另配原生模型服务。
- `model_contexts` 按模型 ID 统一保存；未配置模型默认窗口 `128000`、阈值 `80%`。窗口范围 `1024..2000000`，阈值范围 `10..95`。
- 每个 `agents[]` 可分别设置 `context_window`、`compression_threshold`；缺省或 `null` 逐项继承该模型策略。角色模型为空时沿用其执行器的既有模型继承规则。
- 新任务固化有效策略；设置变更不改写已派发任务。旧记录缺少新增字段时使用兼容默认。

原生执行器使用保守 token 估算；它不是服务端 tokenizer 的精确计数。达到阈值后，真实调用同一模型总结较早对话和工具结果，保留系统约束、当前目标和最新完整工具调用组，随后继续工具循环。压缩不删除 SQLite 原始对话或执行证据。压缩请求也消耗用量；当前目标与必须保留的工具结果仍超出窗口时明确失败。

Codex 的 `thread/start.config` 传递 `model_context_window` 与 `model_auto_compact_token_limit`，后者由窗口乘以百分比计算，压缩由已安装 Codex 处理。每次仍创建独立 Codex thread，应用将同会话历史作为背景带入；尚未实现 Codex thread 的恢复复用。

## 本机显示缩放

设置中的界面缩放范围为 80% 至 120%，默认 95%，立即生效并保存在本机界面偏好。桌面使用 WebView 原生缩放，浏览器预览使用兼容方式。不写入后台模型配置、不影响 API 调用或其他设备。

## 无头 HTTP 与命令行

`harness-daemon` 可独立运行，不需要桌面或 WebView。监听 `127.0.0.1`，沿用私有 `daemon.json` 中的令牌认证及 SQLite 数据；关闭桌面不停止服务。

新增独立 Rust 客户端 `self-agent-cli`，读取同一数据目录元数据，不在命令行传递服务令牌，不跟随重定向，仅连接本机服务。`--data-dir` 或 `HARNESS_DATA_DIR` 可选择实例。所有成功输出为 JSON，错误写入标准错误并返回非零退出码。

```sh
cargo build -p harness-daemon -p harness-cli
cargo run -p harness-daemon
```

另一终端：

```sh
self-agent-cli create "开发会话" /absolute/path/to/project
self-agent-cli sessions
self-agent-cli --wait 600 run SESSION_ID "检查项目并完成修改"
self-agent-cli status TASK_ID
self-agent-cli cancel TASK_ID
self-agent-cli send SESSION_ID "为远程任务生成编排草稿"
```

未安装客户端时可用 `cargo run -p harness-cli -- ...` 或 `target/debug/self-agent-cli`。`settings`、`models` 接收 JSON 文件或 `-` 标准输入；`command` 接收完整指令对象，覆盖全部计划和管理协议。`--command-id` 可指定重试的幂等 ID，`--conversation` 隔离来源的默认会话选择。`--wait` 适用于返回任务 ID 的命令，成功终态退出 0；失败、取消或超时退出非零。等待超时不会取消后台任务。

CLI 使用 `channel_id: "cli"` 的本地来源；完成回调进入已有持久化投递记录，不向通讯平台发送。未来真实通讯适配器与 H5 / App 复用同一指令层，尚未提供公网监听、远程配对或实际通讯平台投递。
