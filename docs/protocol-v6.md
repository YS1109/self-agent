# 第七阶段协议：持久事件与会话续接

日期：2026-10-28。状态：0.7 开发分支第四切片，尚未发布完整 0.7；应用版本号暂仍为 0.6.0。本文描述本分支已实现接口，未列能力继续沿用 [0.6 协议](protocol-v5.md)。

本分支贯通共享 Rust 后台、Native/Codex 执行事件、桌面和浏览器消费、HTTP 恢复及 CLI 跟随，并新增 Codex 主会话续接和 Native 跨任务摘要。已增加执行中补充、明确继续、上下文估算及有界只读区间改动面板；没有人工审批界面，也不扩大工具权限。普通托管登录的原生 thread 复用和Git 状态概览仍有下述部分读取限制，不能称完整 0.7 验收已经结束。

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
| `usage` | `input_tokens`, `output_tokens`, `scope` | 上游明确提供的用量，可为 null；Native 为 `request`，Codex 为 `thread_total`，累计值不能反复相加 |
| `context_compaction` | `state` | 实际压缩状态，包括 `reused` 表示已使用校验过的 Native 历史摘要 |
| `context_estimate` | `estimated_input_tokens`, `context_window`, `compression_threshold`, `source`, `phase` | 本地启发式估算，窗口与阈值来自任务策略；不是服务报告用量 |

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

## Codex 事件与主会话续接

协议依据本机 `codex-cli 0.166.0-alpha.1` 生成的 stable app-server JSON schema 核实；不使用标注实验性的 `history` 或不稳定的 `path` 恢复参数。执行器的实际回复、工具始末、累计用量及压缩通知映射到上述事件；按 thread/turn 标识过滤、重复通知去重。工具输出随完成事件提交，不伪造逐字节命令输出或隐藏推理。

- 主任务使用持久 thread，独立子任务继续 `ephemeral:true`。应用会话、角色、规范项目路径、有效模型/服务配置、上下文策略、CLI 路径/版本和本应用权限策略指纹决定复用；指纹只私有保存，不能视为授权。
- 主任务先只读 `config/read` 与 `account/read(refreshToken:false)` 确认身份，执行进程在任何 thread 操作前再次比较有效配置指纹；指纹包含显式 provider 环境凭据和 `OPENAI_API_KEY` 的内存摘要。不一致直接失败，不向变化后的服务默默发送历史。Codex 自身配置中的静态/环境请求头和令牌同样参与执行输出脱敏，原值不持久化。
- 首次 `thread/start` 引导应用历史一次；随后 `thread/resume` 只带当前目标。只有上一回合明确成功，应用历史恰好为前次输入加已完成回合，且 thread 处于 idle、权限与项目匹配、`thread/turns/list` 最新完成回合恰好等于保存的 turn ID，才恢复。其他 CLI/应用加入的远端回合会使恢复失败，不默默导入外部未知写入。稳定 `turn/start` 没有 expected-last-turn 原子前置条件，因此同一持久 thread 不得由其他客户端并发写入；此检查不是跨客户端排他锁。额外讨论、角色/模型/执行器或策略变化会使关联失效，保留原历史新建。
- `thread/resume` 明确返回错误且尚未发送本回合时，允许一次新 thread 引导；进程断线、未知状态、活动回合、ID/权限不符等不按“恢复失败”自动重试。
- thread ready、发送回合前的 inflight、真实 turn ID 和已完成状态持久化。daemon 重启将未完成关联失效，任务按原规则失败并保留实际文件；不重放可能已写入的回合。
- 当前稳定 schema 不提供可可靠绑定的 ChatGPT workspace/account ID；email 不当作身份凭据。登录态或未知身份不能确认时，明确降级为新 thread + 完整应用历史，不恢复旧 thread。动态 `auth.command` 或 `gateway_oauth` 身份也不复用。持久恢复仅用于可确认身份的独立自定义服务，属于有意保守的兼容边界，不宣称所有登录模式均原生续接。
- 成功恢复不保证模型质量；本切片只用本地协议夹具。继承的 MCP/Skills 仍不是本应用可管理权限，不能用 workspace-write 声称约束远端工具。

## Native 摘要持久化

只给直接主任务保存可复用摘要，子任务继续隔离。`session_contexts` 私有记录摘要、原始历史覆盖条数及前缀 SHA-256、生成任务/时间、有效配置和上下文策略指纹。实际凭据只在内存中参与指纹和脱敏，不存入摘要元数据；摘要正文在保存前脱敏，不返回给浏览器。

- 仅当前任务第一次工具执行前、覆盖完整已知历史的有效压缩结果可持久保存；当前任务的中间工具循环压缩不成为下一任务的缓存。原始消息、结果及命令证据不删除。
- 后续请求为“已验证摘要 + 覆盖边界外的全部原始消息 + 当前目标”。包括取消/失败任务与额外讨论形成的后缀，不能只保留成功回复或漏掉最近消息。
- 覆盖指纹不符、模型/服务/项目/角色/权限或上下文策略变化、切换执行器时失效，回退完整历史重新处理；保存失败或取消不冒充已更新缓存。有效摘要可以随数据库重启恢复。
- 快照新增 `context_states`，公开 `{session_id,executor,state,reason,covered_messages,source_task_id,updated_at}`。Native 状态包括 empty/available/invalidated，Codex 包括 ready/inflight/idle/invalidated；不公开 thread ID、摘要正文、配置指纹或凭据。`source_task_id` 在摘要复用时仍指向真实生成任务。当前压缩进度/失败同时保留在任务事件和终态中，旧摘要可用不等于最近任务成功。

## 桌面与浏览器事件消费

Tauri 使用 Rust 私有桥 `start_event_stream({subscriptionId,after,channel})`、`ack_event_stream({subscriptionId,deliveryId})` 与 `stop_event_stream({subscriptionId})`。Channel 交付 event、snapshot(reset) 或 status 消息，每条有进程内递增 delivery_id；每窗口/订阅维护生命周期，旧 ACK 不释放新订阅额度。单帧最多 512 KiB、权威快照最多 16 MiB，最多 64 条未确认消息及 32 MiB 队列，超过边界停止连接并恢复，不阻塞 daemon；关闭窗口/组件时中止网络读取。

桥只读取本机私有元数据、禁止重定向、重启重读地址/令牌。令牌不交给 JS。浏览器开发预览走原有 Vite 受控同源代理，只允许严格的 `/events?after=<安全整数>`，断开时关闭上游，错误保留 409 恢复语义。

前端以序号去重并有界保留快照请求期间的新事件，避免旧轮询快照覆盖新输出；恢复快照替换而非拼接。慢消费者/缓存超过边界重新取权威快照。1.5 秒快照轮询保留为状态刷新兜底，回复和工具过程由真实事件实时驱动；工具面板最多最近 30 条，明确开始不代表成功，服务用量与估算不混淆。

## 任务补充、排队与继续

继续使用带 `command_id` / 固定 `source` 的 `POST /api/v1/commands`。相同信封只返回原收据；同 ID 不同正文拒绝。新增：

```json
{"type":"steer_task","task_id":"正在运行的主任务ID","content":"保留原有接口，再补一个测试"}
{"type":"continue_task","task_id":"已结束的主任务ID","content":"可选的新要求"}
```

- `steer_task` 只接受正在运行的非演示直接主任务，不能附到另一任务、计划节点或已结束任务。每条最多 8192 UTF-8 字节，每任务最多 32 条、总 64 KiB。收据新增 `supplement_id`；已接收不等于已应用。
- `task.supplements` 为持久列表，旧任务也返回 `[]`。每项含 `id/task_id/content/status/reason/applied_via/created_at/updated_at`。状态为 `pending`（待安全边界）、`delivering`（已领取待确认）、`applied`（执行器确认接收）、`not_applied`（明确未应用）、`unknown`（终态前未获得确认）。Applied 只表示输入交付，不代表模型遵守或工具已改变行为。
- 每批最多领取 8 条，按收据顺序原子转为 delivering。Native 首次只总结先前历史，随后才领取补充；只在完整工具批结束后的模型边界加入，当前原目标和全部补充在压缩时保留。含补充请求取得 2xx 响应头后整批确认，非 2xx 明确未应用；断线不重试请求或工具。
- Codex 在真实 turn ID 已确认后用 `turn/steer`，带 `threadId`、`expectedTurnId` 和文本输入；仅一条 RPC 在途，精确匹配响应 ID 与 turn ID 后确认。静默回合每 100ms 检查补充，读取缓冲保留跨轮询的 JSON/UTF-8 分片。拒绝、取消、完成与迟到 ACK 不会注入下一任务或重发未知请求。
- 终态事务将 pending 转为 not_applied、未确认的 delivering 转为 unknown；重启同样不重放。已应用补充进入后续权威历史；未应用记录只作为明确标注的背景，不作为新任务指令。存在未确认输入时 Codex 关联保守失效。
- 「排队下一条」仍是 `run_agent`，沿用同会话 main_sequence；「停止当前任务」是 `cancel_task`，与暂停计划/取消整图不同。
- `continue_task` 只能引用已完成、失败或取消的直接主任务，创建新任务并保存 `continued_from`，不重新执行旧任务。输入明确携带所选任务的原目标、终态与最多 16 KiB 结果预览；不能用无映射 UUID 让模型猜测目标。重复继续由 `continuation_origin` 锚定原始目标并保留 `continuation_request`，不递归嵌套生成的提示。可选新要求最多 8192 字节，生成上下文最多 128 KiB；原始证据不删除。

CLI 新增 `steer TASK_ID CONTENT`、`continue TASK_ID [CONTENT]`；可使用 `--follow` 跟随目标/新任务，POST 仍只发一次。`changes TASK_ID` 只读 JSON，不接受 `--follow` / `--wait`。

## 持久上下文展示

Native 估算 source=`native_local`，phase=`before_compaction` / `request`，包含本地发送的消息与工具定义。Codex 新 thread 的 source=`codex_local_bootstrap`，恢复时为 `codex_current_input_only`，phase=`before_turn`；只估算本次输入，不包括 Codex 内部历史、系统或工具上下文，不能冒充完整 thread 占用。

任务持久保存最新 `context_estimate`（事件字段加 `updated_at`）和 `context_compaction`（state/updated_at），快照在全局 300 条事件窗口之外仍可恢复。前端优先使用任务字段，实时事件更新这些字段；较早快照与终态迟到事件不得覆盖较新状态。窗口/压缩阈值、本地估算、实际 Usage 以及压缩进度分别标注。

## 有界只读改动报告

任务取得项目写锁、首个执行器写入前，在内存中采集基线；排队期间外部变化不会归入此任务新增。结束、失败或取消后，在释放项目锁前采集尾状态。报告仅称「执行期间观察到的变化」，不保证与外部编辑独占归因。演示任务不伪造改动。

- 改动区间固定使用同一目录能力：保存的绝对根路径逐组件 no-follow 打开，不重新 canonicalize；基线、结束文件扫描和 Git 读取共用固定句柄。排队期间根或祖先变为符号链接时拒绝读取；执行中根路径被替换时不跟随新目标。
- 使用 cap-std / cap-fs-ext 的目录能力与逐级 no-follow 文件访问；隐藏/敏感/依赖目录、符号链接、包含已知凭据的文件排除。执行中才获知的 Codex 凭据会回溯过滤 before/after；原始基线不序列化、不写数据库。
- 文件、字节、目录深度与差异均有界；支持中文路径、增删、相同内容重命名、只读/权限变化。二进制、大文件和超预算内容明确说明，不假造文本 diff。报告自身列出 limits/exclusions/partial。
- 不修改 Git index/config，不执行 stage/reset/revert/commit。**生产采集不启动任何 Git 或其他子进程**：git status 会触发仓库 clean/process filters，不能在授权与沙箱之外执行。第四切片改用 cap-relative no-follow 的有界元数据读取器；不读取用户全局配置，不执行过滤器、hooks、fsmonitor、credential helper、网络或 lazy fetch，不读取外部 alternates。
- `baseline_git` / `current_git` 为刻意不完整的 `partial` 概览：支持 SHA-1 index v2/v3/v4、loose/packed refs、loose/pack v2 索引及带深度限制的 OFS/REF delta 对象。只读取 HEAD commit/tree 元数据，不展示或持久化历史普通文件 blobs。索引列比较已验证原始 HEAD 树与 index 的 mode/OID（A/M/D/U/?）；不独占归因于任务，不把它等同执行区间 diff。
- 工作区列始终未知，另用 `raw_worktree` 展示 equal_bytes/different_bytes/missing/unknown/not_in_index_ignore_unknown/not_present；只比较当前有界文件快照的原始 blob 字节，不应用 Git 换行/编码/clean filter 或忽略规则。未纳入 index 不等于未跟踪，字节相同不证明 Git 干净。冲突、特殊 flags、未读取和不支持的文件类型保持 unknown；missing 需 no-follow 再确认路径确实不存在，不能因扫描遗漏或大小写名称差异推断删除。
- 元数据读取累计最多 64 MiB，单元数据文件最多 8 MiB，index/tree 最多 20,000 entries，单解压对象 4 MiB、对象访问 1024 次、delta 深度 16、tree 深度 24、pack 文件最多 16 个且单文件不超过 4 GiB。压缩须到实际流结束，已使用对象重新核对 SHA-1；不扫描整个 pack CRC/trailer，校验范围不冒充仓库完整性审计。未知必需 index extension、SHA-256、split/sparse index、损坏/超限内容均明确 unknown/unavailable，不按空列表报干净。
- loose ref 只有 no-follow 证明确实缺失才回退 packed refs；后者须完整有界验证、无重复和非法行，才能判断 unborn HEAD。外部/链接 gitdir、commondir、reftable、loose 或 packed replacement refs 均拒绝，不能选择旧 packed 值或读取外部目标。普通项目根没有 `.git` 时只报告根目录未检测到标记，不判断父目录仓库。报告不保证跨外部并发 Git 写入的原子快照。
- 快照只含 `task.changes_summary`，状态 pending/available/unavailable/expired 及可见文件数、排除数、partial 等；数量不是完整项目改动总数。取消终态可能早于报告完成，前端显示待采集而非空 diff。
- 认证 `GET /api/v1/tasks/{task_id}/changes` 按需返回安全报告（files/skipped/baseline_git/current_git 等），或 HTTP 200 的 `{state,task_id,reason,summary}` 不可用/过期状态。不存在任务仍报错误，不当成空报告。
- 每份报告最多 512 KiB，最多保留最新 128 份（详情总量不超过 64 MiB）；过期保留任务和摘要。后台重启丢失内存基线时明确 unavailable，不恢复或重放写入。HTTP、桌面受控桥、浏览器代理和 CLI 共用同一详情接口。

## 验证与剩余切片

默认仅本地 HTTP/stdio 夹具，不调用真实模型。原八组回归保持；`streaming_smoke.py` 覆盖流式/恢复，`continuation_smoke.py` 覆盖续接，`steering_smoke.py` 覆盖补充、ACK、取消及未知交付，`changes_smoke.py` 覆盖已有文件/index 保留、失败/取消改动、重启、锁后基线和恶意 Git filters 不执行；代理另有 `web_stream_smoke.py`。Codex 可执行夹具只在 Unix 运行，Windows 明确跳过该部分但继续 Native 场景。

剩余 0.7 验收边界：托管登录身份的安全 thread 复用、Git 完整转换/忽略语义及未支持存储格式，以及受环境阻止的原生/浏览器视觉验收。实际服务与 GUI 未验证不能写作通过；应用版本仍为 0.6.0，完整 0.7 未发布。切片跟踪见 `acceptance-v0.7.md`。
