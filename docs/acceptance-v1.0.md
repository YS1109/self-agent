# 1.0 可靠性验收跟踪

运行版本仍为 0.6.0。本表只记录开发分支证据，不将独立 helper、三平台编译或模拟服务冒充实际安装/真实模型/系统凭据验收。协议见 [v12](protocol-v12.md)，后续范围见 [iteration-v1.0](iteration-v1.0.md)。

## 第一应用切片

| 范围 | 实际验证 | 边界 |
| --- | --- | --- |
| Store 迁移 | 版本 0 旧事件/配置迁移；单事务失败回滚；较新库拒绝且原字节不变 | 后续 gateway schema 必须另增显式版本 |
| SQLite 一致备份 | 活跃 WAL、实际 owner HTTP/CLI、真实 manifest、同 UUID 不重放、目标不覆盖、容量/截止/损坏检查 | 单影像 128 MiB；不是 RSS 保证或备份来源认证 |
| 离线恢复 | 同 daemon.lock 拒绝活跃后台；摘要确认；恢复后启动真实 daemon；旧任务不重放；多阶段中断回滚、坏 marker 保留阻断 | 协作私有目录，不宣称恶意同用户 ABA / capability SQLite VFS |
| 目录身份 | DataLease 取得后置换普通目录或主文件，managed Store 打开拒绝且两份数据不变；目录、metadata 及 probe 创建使用 pinned handle | Unix 目录置换回归；Windows 运行结果由精确 SHA CI 单列 |
| 恢复权威 | 活动任务/规划/批准/回调/续接失效；命名服务与能力停用/解绑；QQ 绑定、ACL、checkpoint、回复及凭据清理权威撤销 | 历史 sent/unknown 不改写，无 OS vault 操作；H5 另接 |
| 诊断 | 固定字段/聚合；真实私有 canary 不出现；客户端导出再次白名单校验 | 不读取系统凭据；连接验证历史明确未记录 |
| UI / CLI | 灰蓝中文 owner 面板、只读恢复 UUID、单 POST 断连、慢 GET、不确定观察历史；CLI 挂起状态 GET 仍服从 --wait | GUI 点击未验证；无在线恢复或启停/更新按钮 |

## 审查与门禁状态

- 独立 storage/jobs/diagnostics 模块已只读审查。实际应用接线独立审查发现的目录打开/在线备份身份缺口、恢复结构与嵌套 panic、CLI 等待 GET 总截止、QQ 计数及 events 分配器检查均已闭环；当前源码复审通过，最终冻结门禁和精确 CI 仍分别记录，不能拿模块单测代替。
- 初次接线门禁：core 7 / daemon 513 / CLI 34、前端 236、17 个后台 smoke 通过；随后安全审查修复使这些数字成为**历史结果**，不是最终冻结结论。
- 新目录身份回归所在 owner API 7 项及 CLI 2 项定向通过；真实进程 reliability smoke 4 组通过。最终全量计数、精确提交和 CI 在发布时追加，不提前宣称成功。
- 上一已发布 QQ 提交 `b4495d800b36c50d456b37a4bfbf41546318fe38`：push `40780438720` / PR `40780446859` 均为 6/6 成功。它不验证此后未提交的可靠性改动。

## 明确未运行 / 尚未接线

- 真实桌面 GUI；当前 Linux 环境完整 Tauri 本机构建缺 GTK/WebKit。路由提取测试不是完整 Tauri 或 GUI。
- 真实模型、QQ、OS keychain/凭据授权，以及公网 nginx 部署；本轮只用本地夹具。
- 生命周期 helper 21 项、更新/诊断独立 helper 13 项不代表应用已具备 drain/握手/安装器。生产更新禁用，无可信生产发布源/签名身份。
- 三平台实际安装、更新、卸载；真实长历史性能与资源指标。后续记录实测样本和环境，不虚构完成率或时延。

## 独立候选冻结门禁（第一应用切片）

候选明确排除 gateway/H5 隔离代码及未接线 lifecycle/update：core **7**、daemon **527**、CLI **35**、前端 **222**（28 文件）通过；fmt、全 targets Clippy `-D warnings`、后台/CLI/Web 构建、Rust 无头事件桥 **7**、维护路由提取 **1**、Web 流式代理 **3 组**通过。前端复用同一 lockfile 的已安装依赖；候选目录禁用 pnpm 自动安装，不改依赖声明。路由提取只验证匹配逻辑，不冒充完整 Tauri。

**17 个后台 smoke 已全部通过**，包含实际 daemon/CLI 的维护 4 组；使用本候选新二进制执行。当前无未解决本地门禁失败；精确提交和三平台 CI 在发布后另行记录。Linux 下链接临时目录祖先的夹具验证已通过，生产 nofollow 规则没有放松。

## 第一应用切片跨平台 CI 修复

`1e7d537be41bb44b4b067984ee3a15db9f0127ff` 的 push `40788105529` / PR `40788113626` 均终态为 4/6 通过：Ubuntu Rust 与三平台 Desktop 通过，macOS / Windows Rust 未通过，不能记为成功。macOS 的 21 个失败均为测试临时根 `/var` 链接被 SQLite NOFOLLOW 拒绝；测试在选择临时根时规范化，运行时不放松链接拒绝。Windows 暴露真实目录身份检查缺陷：普通文件方式重开 `.` 缺少目录打开语义；现直接复制已固定的目录句柄读取身份，不重新解析路径。另修复夹具读取被 Windows 区域锁保护的 `daemon.lock`；仍核对该锁存在，并逐字节核对所有数据库与恢复材料。

新增两个跨平台回归分别验证空/已有数据库的固定句柄身份与启动预检，以及恢复确实到达提交中断点后再恢复。新 Windows/macOS 实际结果仍须修复提交的精确 CI，不以 Linux 单测代替。

修复候选隔离门禁：链接临时目录祖先下 core **7** / daemon **529** / CLI **35**；fmt、全 targets Clippy `-D warnings` 与 daemon/CLI 构建通过。使用候选构建后复制的固定二进制，**17 个后台 smoke 全部通过**（环境中断前完成前 2 项；确认旧进程已结束后用同一二进制补齐后 15 项）。前端和协议未改，本窄修复未重跑 UI/完整 Tauri；先前 exact 1e7d 的三平台 Desktop 通过属于历史结果。独立窄源码复核通过，后续精确 CI 单独记录。

## 已发布维护修复的精确 CI

`76e76226067854d666b07566b069b6f19ad8c2ff`：push `40791050260` / PR `40791056429` 均 6/6 成功，包含原先失败的 macOS/Windows Rust。它不验证后续 v3 性能代码。网关/H5 基线 `3160a4e2e0b0cf54030c56c43b50c6f8e136680c` 的 push `40792802861` / PR `40792812481` 也均 6/6 成功。

## v3 索引调度切片

独立审查与冻结门禁在本节追加；不能沿用上述基线 CI。新回归覆盖实际暂停队列/会话串行不饿死、5,000 完成任务空队列索引、无隐藏远端检查上限、索引事务更新、v2→v3 失败回滚/幂等、旧备份恢复、伪造索引拒绝且原库不变。

真实性能测量和保留失败详见 performance-v1.0：两组空闲 CPU 约 1.5%，20k 回放两组各 3/3 通过，原 seed/历史摘要不变；5,000 任务 status/单任务各 3/3 仍超客户端容量，runner 如实 exit 2。该性能失败正在后续分页切片处理，不计为全部通过。真实 GUI/OS vault/外部服务/安装发布仍未验证。

索引切片最终冻结：core **9** / daemon **603** / CLI **37** / gateway **17** 全量通过；fmt、全 targets Clippy `-D warnings`、三个后台/CLI/gateway 构建及 **18 个后台 smoke** 通过。smoke 使用冻结源码与构建后私有副本，不受下一片并行构建影响；benchmark self-test / Python 编译通过，独立源码及文档复核闭环。本片没有前端修改，未重跑 GUI/全前端门禁；此前 3160a4e2 Desktop 成功不能替代新 SHA CI。性能容量失败仍如上一段单列，不因回归套件通过而隐去。新提交三平台 CI 发布后另行跟踪。

## v3 基准自测 Windows 清理修复

`03322fedc395c3b4d9e191dc4211c86e96261b44` 的 push `40795769321` / PR `40795775886` 均为 **5/6**：三平台 Desktop、Ubuntu/macOS Rust 成功；Windows Rust 的测试、Clippy、构建与 18 个 smoke 均通过，但最后 Python benchmark 自测清理 `copy.sqlite` 因未关闭连接报 WinError 32，整项仍失败。

窄修复仅给四处 SQLite context 增加 `closing`，保留内部事务上下文的提交/回滚语义，不改运行产品、性能数据、断言或门限。本地 self-test、Python 编译及 tracked connection 检查通过（8 个连接全部显式关闭）；parent 只读窄审查通过。没有重跑无变更的 Rust/前端套件，Windows 实际结果等待此修复提交的精确 CI，不用本地结果冒充。
