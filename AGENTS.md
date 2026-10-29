# Self Agent 项目约定

- 产品界面、系统生成的消息、能力说明、执行进度和错误提示默认使用简体中文。代码、协议字段、模型名、文件路径和用户原文保留原样。
- 用户明确不喜欢绿色。界面采用中性灰底色和蓝色强调，不使用绿色作为品牌色、成功色、图连线或图标颜色；明暗主题保持一致。
- 桌面布局参考 Codex 的会话工作台。保持主对话可用，耗时模型请求和子任务不得阻塞结构化管理指令。
- 日常使用由主 Agent 直接读取项目、修改、验证和回复；子 Agent 编排、计划草稿及流程图用于显式远程编排，不作为普通桌面对话的必经步骤。
- 计划、任务结果和回调状态以 Rust 后台的持久化记录为准。演示模式必须明确标记，不得把模拟步骤描述为真实项目操作。
- 当前开发分支命名服务与系统凭据切片见 `docs/protocol-v9.md`，托管 MCP/Skills 切片见 `docs/protocol-v8.md`，执行控制切片见 `docs/protocol-v7.md`，持久事件、执行器续接与流式协议见 `docs/protocol-v6.md`，0.6 基线协议见 `docs/protocol-v5.md`，编排画布见 `docs/protocol-v4.md`，运行控制与验收见 `docs/protocol-v3.md`，角色与执行器见 `docs/protocol-v2.md`，基础编排协议见 `docs/protocol-v1-draft.md`，实现边界见 `docs/implementation-status.md`。新增能力同步更新说明并验证相关行为。
- 后续分版规划见 `docs/roadmap.md`，0.7 范围与验收见 `docs/iteration-v0.7.md`，0.8 切片计划见 `docs/iteration-v0.8.md`，接手入口见 `docs/handoff-v0.7.md`。这些文档描述待开发目标，不表示能力已经实现；规划不产生部署、外部通讯或扩大权限的授权。

- QQ 开发切片协议见 `docs/protocol-v10.md`，范围见 `docs/iteration-v0.9.md`，模拟门禁与真实平台延期边界见 `docs/acceptance-v0.9.md`；QQ 不是本机管理 API 代理。
- 1.0 版本化存储、备份恢复及诊断见 `docs/protocol-v12.md`、`docs/iteration-v1.0.md` 与 `docs/acceptance-v1.0.md`。schema 迁移必须显式提高 ledger 版本；恢复需要撤销所有执行、远端及凭据操作权威，不自动重放副作用。独立 lifecycle/update helper 不等于已完成应用生命周期或生产发布。
- 0.10 自托管网关/H5 合同见 `docs/protocol-v11.md`，联合门禁与未验证范围见 `docs/acceptance-v0.10.md`。远端仅经白名单 RPC 与本机作用域授权；不得复用 owner bearer、自动重发未知命令、提供任意路径下载或把观察结果误记为已投递。gateway schema 由显式 v2 ledger 管理，恢复必须撤销其配置/配对/凭据与回执权威。

- 有索引调度与 schema v3 见 `docs/protocol-v13.md`，长历史真实失败和同 seed 对照见 `docs/performance-v1.0.md`；分页尚未接线前，不能把空闲 CPU/回放改善当成大快照容量通过。

- 有界工作区/历史读取见 `docs/protocol-v14.md`。新客户端显式 `snapshot?window=1`，不得把局部数组当全集、离窗 ID 当不存在或自动换成其它动作目标；返回/刷新首页复用 `current_cursor`，只有明确重新读取首页才建立新上界。旧无 query 快照保留全量语义或显式失败。

- 本机签名校验/私有暂存与兼容握手见 `docs/protocol-v15.md`、`docs/acceptance-update-v1.0.md`；schema v4 的witness不随task DB备份回滚，恢复不能降低保留下限。verification-only配置不是生产安装授权，任何请求/环境不得临时选信任根或执行artifact。Native超时/不兼容不得自动替换后台，保留原启动观察与明确错误；包夹具不等于OS安装/生产签名/GUI验收。

- 同一后台实例的实际停止合同见 `docs/protocol-v16.md` 与 `docs/acceptance-lifecycle-v1.0.md`。新工作必须先登记真实所有者，再在同 Store 锁下检查 admission；HTTP observer 取消不能丢弃 blocking/OS/进程工作。只有两次 quiesce 与真实 worker 清理均确认才能标记 Stopped；历史结果未知与当前物理清理未知分开，不自动重发或启动替代后台。
