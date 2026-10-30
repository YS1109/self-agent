# 协议 v11 · 自托管网关 / H5（开发切片）

本文件记录已接线的开发 wire 合同。本地联合门禁与独立源码审查已通过；精确提交 CI 和真实部署的边界见 acceptance-v0.10.md。QQ 保持独立 v10；本机 owner HTTP API 不通过网关转发。

## 拓扑与状态归属

`H5 HTTPS → 用户 nginx → loopback gateway ← daemon outbound WSS`。
网关持久化的 SQLite 只含设备注册与凭据验证/撤销目录；浏览器 cookie 会话和有界 in-flight 请求仅在内存。任务、绑定授权、命令回执、文本产物和事件游标由 daemon SQLite 管理。

网关重启使所有浏览器登录失效，重新配对需要新的本机确认；不自动恢复以前的浏览器 authority。有效 cookie 内页面刷新/网络重连可恢复。设备 transport key 为独立高熵凭据，不是 daemon owner token，浏览器永远不接收它。

## 内部 WSS wire

wire version 为 `1`，定义见 `harness-core/src/gateway.rs`（由 core 导出、独立 gateway 与 daemon 共同使用）。每帧至多 256 KiB，未知字段/类型拒绝。设备先发送 `ready {protocol,device_id}`，网关才视为在线。

网关发出 `request {request_id,principal_id,binding_revision,rpc}`；其中 principal/revision 来自网关已验证的浏览器会话，不接受浏览器覆写。daemon 回 `reply {request_id,result:{code,data?}}`，可主动发 `revoked {principal_id}` 关闭相应浏览器流。

`rpc` 白名单：`pair_claim`、`pair_status`、`sessions`、`session`、`task`、`events`、`changes`、`artifacts`、`artifact`、`receipt`、`command`。`command` 仅 `run`、`cancel`、`answer`、`steer`、`continue`；不含 approve/settings/credentials/capabilities/cleanup，也不接受通用 URL 或文件路径。

每个 mutation 带稳定 `command_id`，独立于一次 WS 请求的 `request_id`。同 ID、同 principal/修订/会话/内容只能读回原回执；不匹配拒绝。ACL、实际命令 effects 与回执在同一 daemon 事务提交。

## 故障语义

没有在线 ready 设备时立即 `device_offline`；请求可能已发送后，断连/超时返回 `outcome_unknown`。网关不重放。H5 保留原命令标识并只读 `receipt`；暂未找到回执不证明先前请求永远不会提交，不自动改成失败或再次执行。

连接、帧、请求并发与浏览器会话有界。无离线 queue。心跳独立于 SQLite 处理。活动设备凭据或 principal 撤销时关闭相关连接/流；所有新请求在 daemon 重新检查实时配置和 ACL。已提交任务的停止与清理状态沿用执行控制协议，不将断开浏览器等同于撤回副作用。

H5 已配对后，会话列表、所选会话快照或 SSE 遇到 `device_offline` / `busy` / `internal` / `outcome_unknown` 或网络中断，以 2 秒间隔仅恢复只读请求；重取权威快照后再订阅其游标。`unauthorized` / `forbidden` / 无效请求不自动重连，切会话、退出或销毁取消旧恢复。有效事件确认后移除本次离线提示；未知命令 ID 与只读回执约束始终保留，恢复在线不重发命令或宣称原命令成功。

## 浏览器边界

同源 `/gateway/api/`：会话引导、配对/状态、受限 RPC、事件 SSE、退出。响应 `Cache-Control: no-store`；生产 cookie 为 `__Host-`、Path=/、HttpOnly、Secure、SameSite=Strict、不设置 Domain。写请求同时要求精确 Origin 与会话 CSRF；不开放 CORS。显式 fixture 模式仅允许 numeric loopback HTTP，不能作为生产降级。

输入、响应和事件均有界。事件游标来自 daemon 持久序列，不由网关重新编号；失效游标必须回权威快照，不能猜补。H5 不渲染模型 HTML，文本/代码均转义，不把模型链接自动打开或执行。

## 范围与验证限制

任务文本资源使用不透明 ID，必须每次校验 task→session→principal 授权，支持有界文本而非任意工作区/二进制下载。配对权限分别控制 metadata、结果、文本产物、运行、取消、回答；任务限制必须显式，随后与本机/角色限制取交集。

部署例子只提供 nginx WS Upgrade/HTTP1.1、心跳/空闲时间、API no-cache、SSE no-buffer 与 mutation no-retry 配置。域名、TLS 证书、真实公网连接和部署均未执行。

参考：nginx 官方 [WebSocket proxying](https://nginx.org/en/docs/http/websocket.html)，MDN [Set-Cookie](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Set-Cookie)。具体实现上限与验收结果以下文和 acceptance-v0.10.md 为准。

## 实现的明确上限与交互

- 网关设备目录最多 1024 项；浏览器内存最多 1024 会话，未激活子集最多 256。匿名期限 5 分钟、待本机确认 10 分钟、首次激活后固定 12 小时，同时仍受 daemon grant 到期约束。临时目录验证忙/不可读不等同撤销，不永久清除已认证浏览器；这次请求仍失败关闭。
- 每设备 8 / 全局 64 个在线 RPC；发送队列 16，20 秒 RPC 截止。连接保留至多 256 个已发送但不再观察的 request ID，唯一迟到结果被忽略，不借给新请求，也不恢复被撤销 principal；满时关闭此连接代。设备目录校验并发最多 32，拒绝服务防护仍有限，不宣称公网抗攻击能力。
- SSE 全局 64 / 每 cookie 2 条，队列 16，5 秒积压终止生产。事件只用于触发权威快照刷新，不把 delta 重复拼成消息。`cursor_expired` 必须废弃旧流代和旧高游标，接受权威快照的较低游标（如恢复历史），而非无限重试被拒游标。
- 本机配对确认必须逐项选定会话、view/read_results/read_artifacts/run/cancel/answer、完整预算/工具限制与期限，默认全部动作拒绝。明确 cancel/answer 可作用于该会话的桌面任务；补充/回答要求目标不可变控制不高于 grant，继续任务与目标/本机限制取交集。没有远程 approval。
- daemon 文本资源缓存最多 1024、单任务 8 项、每项最多 64 KiB（UTF-8 边界截断并标记）；生成自已保存结果/命令证据/观察 diff，不读取工作区路径。每次读仍重查授权并用当前已知秘密脱敏，不将旧缓存当作授权。
- H5 原命令未知元数据仅在 sessionStorage 保存 command/session/device/principal ID，不保存目标正文或任何 token。成功回执必须匹配原 command_id；只读未找到或错 ID 不解除未知状态。已失效 cookie 不会继承旧身份的未知命令或权限。
- nginx 例子对 bootstrap/pair 使用独立入口限速，不对 SSE/普通 RPC 使用该低额度；空限速键不计数。API 无缓存、SSE 无缓冲、mutation 不代理重试。

已运行隔离单元、loopback 与实际主程序本地联合验证；这不等于真实公网/TLS、OS 凭据或原生 GUI 验收。原执行环境曾阻止 headless Chromium 启动；2026-10-30 已在 macOS 本机 Chrome 的 390×844 夹具中验证离线恢复、未知命令只读回执和文本转义并检查截图，详见 `review-pr1-2026-10-30.md`。仍未进行真实手机、移动网络或公网部署验证。

### 已接受的桌面任务输入与撤权

由 gateway 新建/继续的任务带固定 gateway 来源；配置、principal 或授权期限失效后停止新工作，清理语义仍按执行控制记录。对于本机独立授权的已有任务，已明确授权并成功接受的补充/回答不会回滚，也不转移任务所有权。撤销浏览器后禁止其后续输入/读取并关闭流，但不自动取消本机任务，避免把 answer/run 授权扩大成 cancel。每次输入均保留稳定 command ID、精确 principal/配置/绑定修订及 answer actor 审计；撤销前后的事务顺序决定是否接受，不存在迟到未授权执行。

文本产物权限不是结果权限的旁路：`read_artifacts` 必须同时明确勾选 `read_results`；关闭结果读取后，Task/Session/Events 仅返回受限状态元数据，不暴露提示正文、执行输出、命令证据或审批参数。

### 结果是授权拉取，不是回调投递

`gateway` 来源任务完成时原子保存任务终态、事件和会话消息，不创建通用 `deliveries` 回调，也不声称网络已发送成功。H5 通过按当前授权校验的读取与 SSE 获取这些记录；离线不会生成待发送队列。既有桌面任务接受远程补充后仍保留其原始来源与回调语义。

本机 gateway 管理 API 最多接收 8 项并发工作，Store/SQLite 操作在有界 `spawn_blocking` 中执行。超过限额即 429，不排无限队列；已接收的凭据工作持有额度直到 claim / OS / finalize 完成，HTTP 断开不丢弃已接收操作或自动重放未知写入。
