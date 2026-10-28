# 协议 v9：命名模型服务与系统凭据

本协议在 v8 托管能力和 v7 执行控制之上增加本机所有者管理的 Native 模型服务。它不授权远程入口修改服务、读取凭据或代理这些接口。当前产品版本号仍为 0.6.0；本开发分支按 0.8 切片交付，不代表已经发布 0.8。

## 配置与选择

- `GET /api/v1/providers` 返回 `providers` 和 `credential_operations`。模型服务只有 ID、修订、名称、地址、模型及绑定状态；没有凭据内容或可直接访问 OS 的引用。
- `POST /api/v1/providers/commands` 接受 `{id, command}`。`id` 为唯一 UUID；相同完整元数据指令幂等，复用 ID 提交不同指令拒绝。
- `save_provider`：`id?`、`expected_revision?`、`definition`。定义含 `name`、`base_url`、`models_endpoint`、`model`、`allow_insecure_http`。新配置默认禁用；更新必须提供当前修订。
- `set_provider_enabled`：`id`、`expected_revision`、`enabled`。`delete_provider`：`id`、`expected_revision`。删除后原 ID 不能重新创建。
- 最多 32 个未删除服务。名称最多 80 字符；模型最多 200 字节；地址最多 2048 字节。禁止 URL 用户信息、查询、片段以及已知凭据。默认 HTTPS；仅 loopback HTTP 无须额外确认，远程明文 HTTP 必须显式确认其风险。
- 模型列表端点可相对配置，但解析后必须与主服务同源；不能把凭据发到另一个协议、主机或端口。修改主地址或模型列表地址都会解除旧凭据绑定，包括同源不同路径。旧凭据成为待清理项，不自动删除。
- `Settings.provider_source="named"` 与 `provider_id` 选择全局服务；默认模型取该服务定义。切换来源不迁移旧手动密钥。Native 角色可指定 `provider_id` 和可选模型覆盖；Codex 角色不能指定应用托管 Provider，其自身配置仍由 Codex 管理。
- `POST /api/v1/models` 的 named 模式只接受服务 ID，不接受临时地址、端点或密钥覆盖。所有者可以发现尚未启用服务的模型，以便先完成配置。服务、修订或凭据变化会取消等待，不自动切换配置。

## 凭据生命周期

专用接口仍要求本机 owner bearer token，且不在模型工具或 QQ/H5 远程管理面暴露：

| 操作 | 请求 |
| --- | --- |
| 设置 / 轮换 | `POST /api/v1/providers/{id}/credential`，`{id: operationUUID, expected_revision, secret}` |
| 解除并删除 | `POST /api/v1/providers/{id}/credential-delete`，`{id: operationUUID, expected_revision}` |
| 清理孤立凭据 | `POST /api/v1/credential-operations/{targetOperationID}/cleanup`，`{id: newOperationUUID}` |
| 查询单次结果 | `GET /api/v1/credential-operations/{operationID}`；也可读取完整注册表 |

密钥为 1–2048 字节、可用于 Bearer header 的 ASCII 可见字符。请求体限 8 KiB；不接受 CLI 参数、URL 参数、日志或配置文件作为专用凭据接口的输入。CLI 通过标准输入传递，界面在提交时清空输入，关闭或切换视图不重试提交。

1. SQLite 先保存只含元数据、私有新引用的操作意图，不保存密钥，也不保存密钥的哈希。OS 写入由后台拥有的独立任务完成，不因 HTTP 客户端断开而重放或丢弃已开始的状态记录。
2. 同一操作 ID 只比较元数据身份；重复请求返回既有状态与 `replayed:true, no_secret_applied:true`，**不表示新输入密钥已经保存，也不声称秘密请求体相同**。客户端遇到不确定网络结果只 GET 恢复，不自动重发 POST。
3. 成功 set 后以预期修订 CAS 换入新引用、递增修订。并发编辑使 CAS 失败时，新凭据成为 `orphaned`，不能绑定到新地址。原引用成为单独待清理项。
4. delete 在调用 OS 前先解除绑定、递增修订。OS 删除失败或结果未知时，旧密钥也不会恢复成当前凭据。
5. OS 拒绝、锁定、不可用、找不到条目均失败关闭；没有明文文件、环境变量或另一种系统存储的自动降级。
6. 已开始但超时/中断的写操作是 `unknown`，不能假装回滚。重启将 `pending` / `inflight` 保守恢复为 unknown，不自动向 OS 重放。明确清理只允许当前未被任何服务引用的条目；清理不能改变原始操作的不确定事实：原 `status` / `reason` 保留，独立 `cleanup_status` 记录清理结果。
7. 普通审计操作上限 1024，未清理条目上限 256；为显式清理另保留 256 次操作额度。已完成审计不静默删除以复用幂等 ID。达到上限拒绝新操作；重复未知清理也可能耗尽保留额度，当前没有自动审计归档。

状态可含 `pending`、`inflight`、`completed`、`rejected`、`unknown`、`orphaned`、`cleanup_required`、`cleaned`。绑定元数据只说明曾经确认写入，不证明当前系统凭据库已解锁或模型仍可用。界面明确显示 `bound_not_checked`，不能把配置就绪当作真实模型测试成功。

## 系统存储与边界

- macOS 使用 Keychain；Windows 使用当前用户 Local Credential Manager；Linux 使用 Secret Service。实现明确选择平台后端，不用库的隐式全局存储或明文 fallback。
- OS 条目的属性只使用固定命名空间、绑定摘要与随机引用；Provider UUID、用途与规范服务地址共同绑定。原始密钥由不可 Debug/Serialize 的私有类型持有，缓冲采用尽力归零。
- OS 访问在独立串行工作线程执行，队列最多 32，等待最多 30 秒。某些 OS API 一旦进入不能强制取消；调用超时不等于系统操作停止。后台保留未知结果，停止时只作有截止时间的排空，不声称杀死 OS 调用。
- 测试替身只在 `cfg(test)` 编译中存在，无生产环境变量或文件开关。测试不读真实 OS 凭据，不登记真实 Provider 或调用付费模型。
- v8 的 MCP `credential_refs` 仍不接受非空配置；后续切片通过下文声明槽接入独立 env 凭据。QQ 专用凭据保持另一个 scope，不借用模型服务密钥。

## 执行、上下文与撤权

- Native 主任务、显式子任务在入队事务中固定服务 ID/修订/定义/模型/私有引用。公开任务只显示 `managed_provider` 元数据；旧任务无该字段兼容，声明有服务但缺私有快照则拒绝执行。
- 执行前只从该引用读取一次私有凭据，生成不可变运行 Settings；不继承旧手动 API key 或额外鉴权 header。每次请求、重试、压缩和等待期间检查原服务修订与启用状态，变更则停止；不会把排队任务改投新服务或使用新密钥继续。
- Native 已知密钥同步用于流式输出、工具结果、证据、项目改动报告、摘要和后续模型历史。完整 JSON 键和值先脱敏再序列化/截断，避免带引号或反斜杠的凭据通过嵌套工具 JSON 泄漏。工具的实际参数与用于模型历史的安全副本分离，不重放已经执行的工具。
- 计划草稿创建时还会固定各 Native 角色的私有 Provider 快照；后续节点沿用该修订，服务仍启用但地址修改或密钥轮换也会阻塞后续节点，不静默切换。内部排队失败只阻塞该节点，不回滚已完成前置任务；相关私有快照以保存点一并回滚。
- 显式编排的 message 作业同样固定服务；生成计划后复制私有绑定，change/review 沿用原计划绑定。服务变更导致后续模型请求失败关闭，不回滚已完成子任务，也不静默改用全局新服务。编排结果落库事务再次检查固定身份，避免响应就绪与撤权竞态生成过时计划。
- Provider 修订参与续接上下文身份；密钥轮换、地址或模型配置变化不会复用旧服务的 Native 摘要身份。Codex 自有认证的既有限制保持不变。

## 验证口径

见 `docs/acceptance-providers-v0.8.md`。本协议描述开发实现；真实系统凭据库解锁、GUI 操作及真实模型服务不可因 mock 测试而标记通过。CI 结果只对其精确 commit 生效。

## 托管 MCP 环境凭据（后续 0.8 切片）

此部分扩展 v8 的 stdio MCP 管理与调用，不授予远程 QQ / H5 配置凭据的权限；不把 Provider 密钥、宿主环境或 Codex 自带凭据借给 MCP。

### 声明与绑定

- `McpServerDefinition.credential_slots` 为最多 8 个不重复、规范排序的槽名，格式为 `[A-Z][A-Z0-9_]{0,63}`。`credential_refs` 仅兼容空对象，客户端不能输入 OS 引用。
- 声明、设置及 CLI 使用同一 Rust 核心校验；禁止启动/加载/代理/证书/宿主配置变量，包括 PATH、HOME、PSMODULEPATH、CORECLR_*、LD_*、PYTHON*、任意 PROXY 后缀等。前端按同一列表约束并有契约测试。原始 stdio 后端仍再次检查环境，不因凭据 UI 而放松规则。
- 私有绑定包含 server UUID、槽名、完整配置摘要；摘要覆盖程序、全部参数、协议、规范槽名，以及固定 cwd/env policy。发现使用独立探测目录，实际调用使用任务固定项目；相对参数及所选项目仍是可信程序输入，不是沙箱。
- 程序/参数/协议/槽集合改变会解除全部旧绑定并保留孤立项清理记录。设置/轮换、解除删除使服务修订递增、旧目录失效；必须再次显式发现，不把旧 key 发给新程序，也不假定新 key 看到相同工具。
- 公开 `credential_states` 只有槽名、configured 与 `bound_not_checked` / `not_configured` 等状态；系统存储是否可读只在实际读取时确认。旧数据库缺字段兼容，非法旧非空 refs 隔离为不可用，不迁入 vault。

### 所有者接口与一次性操作

| 操作 | 接口与请求 |
| --- | --- |
| 设置/轮换 | `POST /api/v1/capabilities/mcp/{serverID}/credentials/{slot}`，`{id, expected_revision, secret}` |
| 解除并删除 | `POST .../credentials/{slot}/delete`，`{id, expected_revision}` |
| 查询原操作 | `GET /api/v1/capability-credential-operations/{operationID}`；不存在为 404 |
| 明确清理 | `POST .../{operationID}/cleanup`，`{id: newOperationID}` |

请求体最多 8 KiB。MCP secret 为 **1–2048 UTF-8 字节、禁止 NUL，可含引号、反斜杠及换行**，不同于模型 Provider 的 ASCII Bearer header 约束。请求不存在可选 owner_kind 或任意引用；新 key 若已存在于程序/参数/目录的已知元数据（含 JSON 文本表示）则拒绝，不将明文参数转为合法存储方式。

SQLite 保存独立 MCP intent/receipt，秘密不进入表、操作摘要或公开回执。相同操作 ID 仅固定元数据，重复响应 `no_secret_applied:true` 不表示新 body 已应用。后台拥有已接收 mutation，客户端断开不丢失意图。claim 只执行一次；超时/重启为 unknown、不自动重放；成功 set 以修订 CAS 换入引用，过期成功为 orphaned。delete 先解除，失败不恢复引用。显式 cleanup 保留原 status/reason，另记 cleanup_status。

审计普通操作 1024、显式清理额度另 256，未清理条目最多 256；不删除历史以重用幂等 ID。注册表新增独立 MCP `credential_operations`（扁平安全操作字段与 slot），不混入 Provider scope。UI 密码不放入 React 状态/URL/本地存储；未知结果只 GET 恢复。CLI 使用 `mcp-credential set SERVER SLOT REV --stdin`，保留换行，不收秘密 argv，记录操作 ID 后只发送一次。

### 实际执行与秘密边界

任务在入队事务中固定各服务的修订、槽及私有 refs。所有声明槽必须已配置；运行前每槽只读一次 vault，读取前后和等待期间检查同一快照；读取失败、取消或撤权不会启动模型/服务，不回退到 Provider 或宿主 key。同一任务的多次 MCP 调用只复用该私有内存快照，不重新解析当前配置。

秘密仅放入指定服务的显式环境；env_clear 规则保持，不放在 argv、模型工具定义或公开授权记录。任务级秘密集合在模型、工具、摘要与改动基线之前安装，审批只显示脱敏参数与完整操作摘要。服务变更/撤权在发现后、tools/call 前再次检查；进行中的未知外部效果仍不重放，进程清理隔离规则与 v8 相同。

Native / MCP 的结构化输出、工具历史及元数据检测使用共享 JSON 文本语义处理：单段最多 **1 MiB**、最多 **4 层** JSON-in-text，键和值先处理再序列化/截断，超解析预算隐藏而非透传。常见 JSON 字符串及 ASCII Unicode 转义模式也在流式分块/展示截断前处理；匹配器每次完整处理预编译复用，扫描项目时同样复用。它不是任意编码解码器，不承诺识别 base64 等自定义编码。改动报告排除包含已知凭据的文本及标准 JSON 表示，包括晚发现的秘密。

联合验证见 [MCP 环境凭据验收](acceptance-mcp-credentials-v0.8.md)。真实 OS 授权、真实 MCP 服务和 GUI 未运行；本切片不代表已完成真实平台 enrollment。
