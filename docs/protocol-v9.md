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
- v8 的 MCP `credential_refs` 仍不接受非空配置；其 env 凭据绑定与 QQ 专用凭据是后续独立接线，不借用模型服务密钥。

## 执行、上下文与撤权

- Native 主任务、显式子任务在入队事务中固定服务 ID/修订/定义/模型/私有引用。公开任务只显示 `managed_provider` 元数据；旧任务无该字段兼容，声明有服务但缺私有快照则拒绝执行。
- 执行前只从该引用读取一次私有凭据，生成不可变运行 Settings；不继承旧手动 API key 或额外鉴权 header。每次请求、重试、压缩和等待期间检查原服务修订与启用状态，变更则停止；不会把排队任务改投新服务或使用新密钥继续。
- Native 已知密钥同步用于流式输出、工具结果、证据、项目改动报告、摘要和后续模型历史。完整 JSON 键和值先脱敏再序列化/截断，避免带引号或反斜杠的凭据通过嵌套工具 JSON 泄漏。工具的实际参数与用于模型历史的安全副本分离，不重放已经执行的工具。
- 计划草稿创建时还会固定各 Native 角色的私有 Provider 快照；后续节点沿用该修订，服务仍启用但地址修改或密钥轮换也会阻塞后续节点，不静默切换。内部排队失败只阻塞该节点，不回滚已完成前置任务；相关私有快照以保存点一并回滚。
- 显式编排的 message 作业同样固定服务；生成计划后复制私有绑定，change/review 沿用原计划绑定。服务变更导致后续模型请求失败关闭，不回滚已完成子任务，也不静默改用全局新服务。编排结果落库事务再次检查固定身份，避免响应就绪与撤权竞态生成过时计划。
- Provider 修订参与续接上下文身份；密钥轮换、地址或模型配置变化不会复用旧服务的 Native 摘要身份。Codex 自有认证的既有限制保持不变。

## 验证口径

见 `docs/acceptance-providers-v0.8.md`。本协议描述开发实现；真实系统凭据库解锁、GUI 操作及真实模型服务不可因 mock 测试而标记通过。CI 结果只对其精确 commit 生效。
