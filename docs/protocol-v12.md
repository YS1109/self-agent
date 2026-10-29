# 协议 v12：版本化存储、备份恢复与诊断（1.0 第一应用切片）

这是开发分支的存储维护协议，不是 1.0 发布声明。包版本仍为 0.6.0。生命周期、客户端兼容握手、自动更新及安装包验收另行实现；没有生产签名、发布或部署。

## 1. 数据目录与 schema

- 后台启动先取得私有数据目录的 `daemon.lock`，处理既有恢复 journal，再以只读 `SQLITE_OPEN_NOFOLLOW` 连接检查 `user_version` 和 `storage_migrations`。检查成功后才打开可写 Store、运行迁移和恢复。
- 数据目录须为绝对本地路径；逐个组件拒绝链接，Windows 拒绝非固定本地驱动及重解析路径。新 Unix 目录使用 0700；既有目录不擅自改权限，Windows 使用继承 ACL，不声称等价于 Unix 权限。所有者应选择自己控制的私有目录。
- 本协议第一应用切片的 schema 为 **1**，ledger identity 为 `self-agent-schema-v1-atomic-baseline`。没有 ledger 的版本 0 是历史基线；当前全部建表、旧事件有序序号转换、能力/服务/QQ schema 和备份任务表在一个事务内迁移，失败回滚。未来新增 schema 必须显式提高版本并定义迁移，不能继续偷偷建表。
- 较新版本、缺失/冲突 ledger、不可安全只读检查的 hot journal 均拒绝启动，不重建或删除原数据库。只读检查不执行应用迁移；WAL 的共享内存协调不代表所有 SHM 字节永远不变。

## 2. 所有者 HTTP 接口

沿用本机 owner bearer 认证。QQ/H5 远程来源不获得本机 token 或这些路径。

| 方法与路径 | 作用 |
| --- | --- |
| `GET /api/v1/reliability` | schema、备份可用性、离线恢复模式、更新禁用状态、最多 256 条备份回执 |
| `GET /api/v1/reliability/diagnostics` | 固定字段的诊断 JSON，最多 16 KiB |
| `POST /api/v1/reliability/backups` | `{id,parent,name}` 创建备份意图；请求上限 8 KiB |
| `GET /api/v1/reliability/backups/{id}` | 查询原 UUID 回执；不存在返回 404 |

`id` 为规范 UUID；`parent` 为现有绝对目录，UTF-8 最多 4096 字节，不接受 `.`/`..`；`name` 为 1–80 个 ASCII 字母、数字、`-`、`_`。只新建目标目录，绝不覆盖。最多两个在线工作槽；满载返回 429 且不创建意图。最多保存 256 条持久操作审计，满后拒绝新意图，不静默淘汰未知结果。

同 UUID、同参数只返回既有回执；参数不同拒绝。意图与领取 token 先提交，随后独立 worker 执行，HTTP 断连不撤销、不重发。重复 POST、重启、UI 重开都不重新执行文件操作。重启将 pending/running 转为 unknown。

公共回执只有 `id,state,reason,manifest`，不包含目的路径或私有领取 token。状态为 pending/running/completed/failed/unknown；completed 必须对应实际持久 manifest。失败或未知不能当作“目标目录没有文件”，应检查原目录。客户端在不确定时仅 GET 原 UUID，不自动重试 POST。

## 3. 一致备份

使用 SQLite backup API 和序列化生成一致影像，不直接复制活跃数据库与 WAL 文件对。源连接由持有数据目录锁的 guard 自己只读打开；在线 lease 固定成功打开 Store 时的主文件身份（首次建库也固定），固定目录及该主文件身份在快照前后复核，不能把替换目录的数据误当原数据。支持活跃 WAL 写入。

每个数据库影像最多 **128 MiB**，每步最多复制 64 页并检查体积和 deadline，在线请求 deadline 为 30 秒；此限制不是整个进程 RSS 的上限。完整性、外键、支持的 schema 对象均检查；不接受触发器、视图、虚拟表。取消/超时检查覆盖快照完成及 manifest 发布前；不能抢占的系统调用结束后不得返回迟到成功。

备份目录包含 `database.sqlite` 和最后发布的 `manifest.json`，记录格式/schema/长度/SHA-256。私有新文件、完整写入与同步后才算完成。失败时尽力清理本次新建的不完整目标；清理失败或结果未知仍不能推断目录不存在，不覆盖或自动重试。

**备份不是脱敏诊断**：它保留会话、项目路径、任务输入输出及历史私有元数据；历史内容可能敏感。应存放在所有者控制的目录，不自动上传。SHA-256 检测损坏，不认证来源或证明备份可信。

## 4. 离线恢复与中断

CLI：

```sh
self-agent-cli backup-inspect /absolute/backup
self-agent-cli --data-dir /absolute/data restore /absolute/backup --confirm-sha256 <完整小写摘要>
```

恢复不会停止后台；活跃 `daemon.lock` 直接拒绝。先检查原库兼容性，再验证备份 manifest、摘要、长度、SQLite 完整性和外键、迁移及应用记录；只在隔离数据库内准备。应用结构检查包括必需列顺序/类型/主键、关键唯一约束、events 的可信 AUTOINCREMENT DDL 形状和共享业务类型，并兼容已有显式 serde defaults。它不是任意 SQL schema 的语义等价验证；额外 CHECK/FK/default 等未支持差异不能据此视为可信。只应恢复所有者自行产生、独立确认可信来源的备份；不支持把不可信第三方数据库当安全导入格式。恶意/损坏 FIFO、链接和异常文件类型被拒绝，不因只读打开 FIFO 永久等待。

替换前生成一致旧影像、新影像与 journal，逐阶段同步并保留原主库/WAL/SHM。中断后依据严格匹配的 journal/commit marker 确认完成或回滚；错误摘要、截断 marker、缺失/冲突材料阻断并保留，不猜测“已提交”。目录身份检查适用于恢复全过程。本机制防协作进程误用与目录置换，不宣称抵御恶意同用户的任意 ABA 操作或提供 SQLite capability VFS。

恢复成功会撤销运行权威而保留历史事实：

- 原排队/运行/等待任务终止，`stop_reason=backup_restored`；未确认清理保留 unknown，不重放写入。
- 活动计划暂停，规划请求、批准/问答、回调与能力调用不自动重放；摘要/thread/turn 关联失效。
- 命名服务和 MCP/Skills 禁用，解除活跃凭据引用；系统凭据审计不具备恢复后执行删除/清理的权威，不读写 OS vault。
- QQ 配置停用，绑定、配对、ACL、选择、checkpoint、回复资格撤销；历史 routes、sent/unknown 审计保留；pending 不发送、sending 转 unknown。恢复中的凭据操作不重放，也不自动删除外部秘密。
- 备份操作 pending/running 转 unknown；备份自身包含的在途回执也不重新导出。

后续已接入 H5/gateway 的 schema v2 迁移与恢复撤权，见 protocol-v11；当前索引调度 schema v3 见 protocol-v13。该后续实现不改变本节不重放和保留未知事实的原则。

## 5. 诊断与界面

诊断只允许固定应用/版本/平台/schema/存储/vault/update 状态和聚合计数。禁止原始 snapshot、标识符、路径、URL、提示词、工具输出、日志或秘密；不探测/解锁系统凭据库。`vault=not_checked` 不等于可用；`providers_configured`、连接验证、任务成功分别表达，当前连接检查不持久化，明确 `connection_check_history=not_recorded`，验证计数不从任务成功推断。

桌面/Web 的“备份与诊断”复用 owner HTTP。目的参数仅在当前表单内；本机恢复观察只保存 UUID，不保存路径或凭据。显式结束未知观察须确认旧操作可能仍在运行/已写入；旧 UUID 留在最多 64 项本机观察历史，满载拒绝新观察而不静默丢弃。结束观察不是取消、完成或重发后台操作。诊断下载前再次严格校验白名单。CLI `backup --wait` 仅查询状态，不因超时取消或重发；`backup-inspect`/`restore` 离线执行，无需 owner HTTP token。

## 6. 仍未宣称完成

这一步没有完整后台 drain/start/stop/restart UI、客户端协议拒绝、持久更新降级下限、生产可信更新源或安装器。独立 lifecycle/update helper 测试不是实际应用验收。真实 GUI、系统凭据授权、安装升级/卸载和真实外部服务验证单列；三平台编译和单测不替代这些实机验证。
