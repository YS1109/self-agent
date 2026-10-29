# 0.7–1.0 路线图验收追踪矩阵

更新：2026-10-29。本文把[路线图](roadmap.md)的要求映射到实际实现和证据，**不是 1.0 发布完成声明**。版本标题、独立 helper 测试和历史 CI 均不能代替当前提交的应用验收。

## 1. 范围、基线与判断方法

- 已发布开发分支 HEAD：`925e6cf2d924141a0cff972113d5b37556891de9`（v16 实际 lifecycle，tree `29fb0f4b0ce4c42cada299f773a5ad1e5ff5b251`）；前序 v15 为 `7aea4f15`，生产等价 Windows cfg 修复为 `43d12ee3`。分支 `codex/v0.7-native-events`，[草稿 PR #1](https://github.com/YS1109/self-agent/pull/1)。用户本地审核和合并；没有 merge、tag 或公开 release，运行包版本仍为 `0.6.0`。
- 本文依据该基线源码、各版本 acceptance/protocol、固定候选测试记录和性能归档。**编写本文没有重新执行测试**。下述 CI 状态是主任务提供的精确 run 观察，不是从版本名或本地结果推断。
- v15 更新校验、兼容握手、启动体验与 v16 实际 lifecycle 已在上述开发分支发布；截至 2026-10-29 06:34 UTC，v16 两轮精确 CI 均 **4/6**，窄跨平台修复已完成本地门禁并随本文提交，仍待新精确 CI。已发布不等于 CI 全绿或生产可发行；详见第 6 节。
- 用户选择 QQ 官方机器人、自托管 nginx、先完成 H5、只做模拟测试。可开发/构建/推送开发分支及运行 CI；不因此获得真实模型消费、真实 QQ 收发、OS 凭据登记、生产部署或发布授权。
- 可选 **0.11 独立 App 已按用户指示排除**：不做 iOS/Android 原生 App、商店分发或系统推送，也不把它列成 1.0 阻塞。复杂图节点、模板库、并行 worktree、更多执行器、Responses、外部历史导入等仍是路线图候选，不偷偷并入本轮。

状态用语：**已实现 / 模拟通过**＝真实应用路径已接线，本地模型/协议/内存 vault 夹具通过；**部分 / 有边界**＝有明确未实现能力或不支持场景；**接线中**＝工作树而非发布基线；**实机待验**＝代码存在，但指定外部环境未验证。它们不能合并成一个“全部完成”百分比。CI Desktop 成功也不等于 GUI 点击、安装器或系统凭据授权通过。

### 当前不能关闭的事项

1. **最新 v16 精确 CI 待修复**：`925e6cf2` 的 push `40828936545` / PR `40828942266` 各 **4/6**，三平台 Desktop 和 Ubuntu Rust 成功；macOS daemon **790/791**（僵尸进程组信号探测夹具），Windows daemon **728/730**（两个新 CMD / Job 复合脚本夹具）。窄修复已独立复核并重新运行本地门禁，仍待新精确 CI，不能以 Linux 结果声称跨平台全绿。前序 v14 lint / macOS 网关竞态及 v15 Windows lint / cp1252 输出失败继续保留在证据索引和第 6 节。
2. **启动等待修复已发布，但不是性能优化或实机验收**：原 5,000 任务温启动 7.153 / 6.836 / 5.702 秒，2/3 超过当时 Native bridge 的 6 秒截止。v15 已发布单次启动、可见进度、原始 30 秒截止和迟到只读恢复；真实 6.5 秒子进程回归通过，固定 v15 daemon 对原 5,000 任务 seed 副本的首次 schema 1→4 与三次温启动 compatible readiness 分别为 11.599 / 20.335 / 3.523 / 3.166 秒，完整历史和原 seed SHA 不变。保留慢样本，不称数据库加速、任意负载 SLA 或 GUI 验收；最新 CI 尚未全绿。
3. **managed ChatGPT Codex 不能声称持续 resume 已全部完成**：公开接口没有经核实可用的不可变账户 principal 合同；已实现保守 fresh thread + 应用历史回退，不读取 auth 文件、不拿邮箱/工作区名冒充稳定身份。可验证自定义服务身份的续接与这一限制分开计。
4. **1.0 发布条件未齐**：实际三平台安装/升级/卸载、GUI、真实 OS vault、生产签名身份/更新源未验或未配置；v15/v16 开发分支接线和模拟结果不能提前算生产交付。nginx 部署由用户负责，本轮没有公网部署。

## 2. 证据索引：精确提交与门禁

表中 CI 仅证明对应提交；不同批次、平台和独立 harness 的测试数有重叠，不相加。后续提交包含前序能力，不意味着可以把后续 CI 写成前序 SHA 的 CI。

| 证据 | 实现提交 / 固定范围 | 已记录验证与精确 CI |
| --- | --- | --- |
| E07 流式、续接、补充 | `bb0b7e08` 首切片；`c013d740` / `b97d1f71` 续接与桌面；`4d77bab2` / `7142474c` 补充与差异 | [0.7 验收](acceptance-v0.7.md)；首片 [push 40721056695](https://github.com/YS1109/self-agent/actions/runs/40721056695) / [PR 40721141000](https://github.com/YS1109/self-agent/actions/runs/40721141000) 成功；`b97d1f71` [push 40736034504](https://github.com/YS1109/self-agent/actions/runs/40736034504) / [PR 40736044231](https://github.com/YS1109/self-agent/actions/runs/40736044231) 各 6/6；`7142474c` [push 40746966975](https://github.com/YS1109/self-agent/actions/runs/40746966975) / [PR 40746977940](https://github.com/YS1109/self-agent/actions/runs/40746977940) 各 6/6。后续 `83603bc8` 有界 Git、`a362982e` 身份安全回退纳入当前源码，不能借用前一 SHA 的 CI。 |
| E08 执行控制 | `970fec28` / `4c6132ba`；后续 `d4bf5843` 修复 Codex 输出额度配对 | [执行控制验收](acceptance-v0.8.md)；`4c6132ba` [push 40755483277](https://github.com/YS1109/self-agent/actions/runs/40755483277) / [PR 40755495889](https://github.com/YS1109/self-agent/actions/runs/40755495889) 各 6/6；后续配对修复 `d4bf5843` 的 [push 40759118072](https://github.com/YS1109/self-agent/actions/runs/40759118072) / [PR 40759126511](https://github.com/YS1109/self-agent/actions/runs/40759126511) 也均成功；本轮已重新只读核验其精确 SHA 与终态，当前冻结 smoke 重验。 |
| E08-C MCP / Skills | `c8780b91`，跨平台夹具修复至 `7cd1b16c` | [能力验收](acceptance-capabilities-v0.8.md)；`7cd1b16c` [push 40766271204](https://github.com/YS1109/self-agent/actions/runs/40766271204) / [PR 40766281131](https://github.com/YS1109/self-agent/actions/runs/40766281131) 各 6/6，Windows daemon 221、macOS 272、15 个后台 smoke。 |
| E08-P 命名服务 / vault | `675c326d46c8fce6cb5f5c333d7b7cbb5a01131a` | [服务验收](acceptance-providers-v0.8.md)；[push 40771665493](https://github.com/YS1109/self-agent/actions/runs/40771665493) / [PR 40771672539](https://github.com/YS1109/self-agent/actions/runs/40771672539) 各 6/6；Windows daemon 271、macOS 322。实际路由及模型夹具使用内存 vault，不是 OS vault 实测。 |
| E08-M MCP 环境凭据 | `e90e936b`，Windows UTF-8 夹具修复 `498bc7ed` | [凭据验收](acceptance-mcp-credentials-v0.8.md)保留原失败：[push 40777667904](https://github.com/YS1109/self-agent/actions/runs/40777667904) / [PR 40777678427](https://github.com/YS1109/self-agent/actions/runs/40777678427) 的 Windows 新测 293 通过 / 3 失败。修复 `498bc7ed` 的 [push 40778392030](https://github.com/YS1109/self-agent/actions/runs/40778392030) / [PR 40778400565](https://github.com/YS1109/self-agent/actions/runs/40778400565) 均成功；本轮已重新只读核验精确 SHA 与终态，仍保留原失败，不借后续提交代替该证据。 |
| E09 QQ | `b4495d800b36c50d456b37a4bfbf41546318fe38` | [QQ 验收](acceptance-v0.9.md)；114 项 QQ 定向（包含实际 Store 15），6 组真实进程 QQ smoke，最终 16 个后台 smoke；[push 40780438720](https://github.com/YS1109/self-agent/actions/runs/40780438720) / [PR 40780446859](https://github.com/YS1109/self-agent/actions/runs/40780446859) 各 6/6。全部 QQ 网络为 loopback 夹具。 |
| E12 存储可靠性 | `1e7d537b`，跨平台修复 `76e76226` | [1.0 验收](acceptance-v1.0.md)：原 [push 40788105529](https://github.com/YS1109/self-agent/actions/runs/40788105529) / [PR 40788113626](https://github.com/YS1109/self-agent/actions/runs/40788113626) 各 4/6，保留 macOS/Windows 失败；修复提交 [push 40791050260](https://github.com/YS1109/self-agent/actions/runs/40791050260) / [PR 40791056429](https://github.com/YS1109/self-agent/actions/runs/40791056429) 各 6/6。 |
| E10 网关 / H5 | `3160a4e2e0b0cf54030c56c43b50c6f8e136680c` | [H5 验收](acceptance-v0.10.md)：core 9 / daemon 595 / CLI 37 / gateway 17 / UI 265、18 个后台 smoke；真实 daemon + gateway + 模型夹具 5 组；[push 40792802861](https://github.com/YS1109/self-agent/actions/runs/40792802861) / [PR 40792812481](https://github.com/YS1109/self-agent/actions/runs/40792812481) 各 6/6。 |
| E13 索引调度 | `03322fed`，Windows 基准清理修复 `6f497bae` | 原 [push 40795769321](https://github.com/YS1109/self-agent/actions/runs/40795769321) / [PR 40795775886](https://github.com/YS1109/self-agent/actions/runs/40795775886) 各 5/6，Windows 最后 Python 自测清理失败，不掩盖；修复 [push 40798776743](https://github.com/YS1109/self-agent/actions/runs/40798776743) / [PR 40798784130](https://github.com/YS1109/self-agent/actions/runs/40798784130) 各 6/6。旧 5,000 任务全量容量失败仍留在[性能记录](performance-v1.0.md)。 |
| E14 有界历史 / 原本地冻结 | `7b026c06bd475aeaf50f3141dc5a0369c7d623b9` | [1.0 验收](acceptance-v1.0.md)：core **9** / daemon **623** / CLI **48** / gateway **17** / UI **315**；fmt、Clippy、TypeScript、Web/mobile 构建、本地 **19 个后台 smoke**；Native 桥提取 **17**、实际 Web 代理 **3 组**及真实响应→TS parser 通过。精确 [push 40801817466](https://github.com/YS1109/self-agent/actions/runs/40801817466) / [PR 40801824633](https://github.com/YS1109/self-agent/actions/runs/40801824633) **各 3/6，三平台 Desktop 成功，Rust 1.99 新增 Clippy `chunks_exact_to_as_chunks` 阻断，后续测试未运行，未全绿**。本地工具链通过不能替代远端新工具链结果。 |
| E14-F 新 lint 窄修复 | `03341d39246689eefdacfcb51271078f9f98e7f5` | 仅固定二字节 iterator 等价 API 与验收记录；隔离 Store 16 / 真实 HTTP 3 / fmt / daemon Clippy 通过，源码复审通过。[push 40803221173](https://github.com/YS1109/self-agent/actions/runs/40803221173) / [PR 40803228478](https://github.com/YS1109/self-agent/actions/runs/40803228478) 分别 **6/6 成功 / 5/6**，后者唯一 macOS smoke 竞态为撤权帧先结束在途只读 RPC、返回 `outcome_unknown`，旧夹具却只接受立即拒绝。后续 E14-G 保持所有观察无 data，并在固定 10 秒内确认最终拒绝；不抹去本行失败。没有将旧 E14 全套或性能冒充本次重新执行。 |
| E14-G 网关撤权观察窄修复 | `c73a62afe446ed089ff6a80087b447c5eb7610de` | [push 40821487804](https://github.com/YS1109/self-agent/actions/runs/40821487804) / [PR 40821493624](https://github.com/YS1109/self-agent/actions/runs/40821493624) 均 **6/6 成功**；不回写 E14/E14-F 原失败，也不代替后续 v15/v16。 |
| E15 校验/暂存与兼容启动 | `7aea4f15`；Windows cfg 窄修复 `43d12ee3` | 开发分支已发布；两提交各自两轮 CI 均终态 **5/6**。前者仅 Windows `unused_mut` 阻断、该平台后续 tests 未跑；后者 Windows 产品 Rust / update 4 组通过，仅最后 package smoke 出现 cp1252 中文输出失败；macOS / Ubuntu Rust 全部 smoke 与三平台 Desktop 成功。原始失败与本地证据见 [更新验收](acceptance-update-v1.0.md)，不宣称完整 CI 通过。 |
| E16 实际后台停止 | `925e6cf2d924141a0cff972113d5b37556891de9` | 原本地固定候选 core13 / daemon791 / CLI53 / gateway17 / UI343、22 个 smoke、Web 4 组通过；[push 40828936545](https://github.com/YS1109/self-agent/actions/runs/40828936545) / [PR 40828942266](https://github.com/YS1109/self-agent/actions/runs/40828942266) 均终态 **4/6**，macOS / Windows Rust 失败见第 6 节及 [生命周期验收](acceptance-lifecycle-v1.0.md)。窄修复尚未提交。 |

## 3. 固定产品方向与 0.7

为便于阅读，未写完整前缀的 Rust 文件相对 `crates/harness-daemon/src/`，`components/` 相对 `src/`，Python smoke 相对 `scripts/`；其余写完整仓库路径。测试入口与 E 编号共同形成证据，不以文件存在本身断言行为正确。

范围及合同：[0.7 迭代](iteration-v0.7.md)、[协议 v6](protocol-v6.md)、[实施状态](implementation-status.md)。

| ID / 路线图要求 | 实际代码及可复现证据 | 当前判定 / 未关闭边界 |
| --- | --- | --- |
| F01 共享 Rust 后台、SQLite、桌面 / HTTP / CLI | `crates/harness-daemon/src/lib.rs`、`store.rs`、`crates/harness-cli/src/main.rs`、`src-tauri/src/bridge.rs`、`src/api.ts`；`headless_smoke.py`、`web_stream_smoke.py`、E14 联合门禁 | **已实现 / 模拟通过**。不是各客户端各自维护执行状态；Native 桥提取测试不是完整 Tauri GUI 实机。 |
| F02 主 Agent 日常直接执行；显式远程 DAG 兼容 | `provider.rs`、`codex.rs`、`coordinator.rs`、Store 主任务 / 计划命令；`provider_smoke.py`、`orchestration_smoke.py`、`plan_change_smoke.py`、QQ 受限编排 smoke | **已实现 / 模拟通过**。普通目标无需先生成计划；未扩展候选图节点、worktree 等。 |
| F03 配置 / 角色 / 上下文固化，管理不等待模型 | `configuration.rs`、`context.rs`、`store/providers.rs`、`execution_control.rs`；`configuration_smoke.py`、预算 / QQ 阻塞模型期间查询取消回归 | **已实现 / 模拟通过**。模型/角色不能扩大权限；任务持有有效配置，不热换 endpoint/key。 |
| F04 简体中文、灰蓝明暗主题、可调缩放 | `src/App.tsx`、样式、设置及组件测试；既有桌面 UI、E14 Web/mobile 构建 | **代码及组件验证存在，视觉实机待验**。不以 jsdom、SSR 或 Desktop CI 声称已检验三平台主题/缩放/WebView。 |
| 07-P0-1 Native / Codex 增量回复和统一执行事件 | `provider_stream.rs`、`provider_redaction.rs`、`codex/stream_events.rs`、`src/event-stream.ts`、`components/task-stream.tsx`、Native `event_stream.rs`、CLI follow；`streaming_smoke.py`、`web_stream_smoke.py`、E07/E14 | **已实现 / 模拟通过**。SSE 分片、UTF-8、完整工具参数及终止语义验证后才执行；JSON 兼容响应不触发第二次 POST。工具/命令输出目前是完成后有界记录，**不是逐字节 stdout/stderr 终端直播**。 |
| 07-P0-2 持久游标、补发、快照、慢消费者 | `store.rs` 持久事件、HTTP SSE、CLI reconnect、TS reducer；断流/过期游标/重启/旧 token 元数据间隙回归，E07/E14 | **已实现 / 模拟通过**。单调序号、订阅后补库、终态与 outbox 原子化；20,000 保留事件、分批补发、超期明确恢复，不承诺永久完整审计。流文本有 256 KiB 上限及明确截断标记。 |
| 07-P0-3a Codex 多轮 / 重启续接 | `codex.rs`、`codex/config_identity.rs`、`store/continuation.rs`；`continuation_smoke.py`、配置/账户/项目/角色/模型不符回归；E07 及后续身份修复 | **部分 / 安全边界明确**。可核对自定义服务身份时复用 thread；恢复失败不重放未知写入。managed ChatGPT / 无稳定身份合同采用 fresh thread + 应用历史，**不算该场景 thread resume 已完成**；真实 Codex 登录/模型未测。 |
| 07-P0-3b Native 跨任务摘要 | `provider.rs`、`context.rs`、`continuation.rs`、`store/continuation.rs`；三轮、重启、配置变化、取消/失败历史、前缀指纹与新消息边界回归，`continuation_smoke.py` | **已实现 / 模拟通过**。只持久压缩本轮执行前的既有历史；当前目标及后续消息不遗漏，半轮工具压缩不冒充完整历史；无摘要失败时原历史保留。 |
| 07-P1-1 补充当前任务、排队、停止后继续 | `steering.rs`、`store/steering.rs`、Native / Codex steering、`execution-controls.ts`；`steering_smoke.py`、ACK/拒绝/断线/取消/重启回归 | **已实现 / 模拟通过**。pending/delivering/applied/not_applied/unknown 可见；applied 是请求已接收，不是模型服从。Native 在请求边界注入并保留用户目标，不强迫额外模型回合；继续新建任务，不重放原写入。 |
| 07-P1-2a 本轮改动与已有改动区分 | `changes.rs`、`changes/git.rs`、`changes/git/objects.rs`、`store/changes.rs`、`components/task-changes.tsx`；`changes_smoke.py`、固定目录句柄 / 非 Git / 既有修改回归 | **已实现但为有界观察**。任务区间基线与最终文件差异、已有 staged/unstaged/untracked 提示分开。只读解析 Git 元数据，不运行 status/filter/hooks/fsmonitor/fetch；过滤/编码/特殊格式、超大/二进制/遗漏明确 unknown/partial，不能声称完全等价 Git 或独占归因所有外部编辑。 |
| 07-P1-2b 上下文估算与实际用量 | `context.rs`、Native `ContextEstimate`、Codex Usage、`components/model-context-settings.tsx` / 任务面板 | **已实现 / 模拟通过**。本地估算、压缩前后、单请求 usage 与 Codex thread_total 分开标记；不把估算当供应商账单或 Codex 完整内部上下文。 |

## 4. 0.8、0.9、0.10

范围及合同：[0.8 迭代](iteration-v0.8.md)与协议 [v7 执行控制](protocol-v7.md) / [v8 能力](protocol-v8.md) / [v9 凭据](protocol-v9.md)；[0.9 迭代](iteration-v0.9.md)与 [v10 QQ](protocol-v10.md)；[0.10 迭代](iteration-v0.10.md)与 [v11 网关](protocol-v11.md)。

| ID / 路线图要求 | 实际代码及可复现证据 | 当前判定 / 未关闭边界 |
| --- | --- | --- |
| 08-P0-1 模型轮次、时长、重试、输出预算 | `execution_control.rs`、`store/execution.rs`、`provider.rs`、`codex.rs`；`budget_smoke.py` 13 组、`execution_control_smoke.py`、E08/E14 | **Native 已实现 / 模拟通过**：压缩/修复/重试计入预算，耗尽保留证据。**Codex 有边界**：外层时长/输出可控；无法强制的内部模型/修复/重试或工具权限限制在启动前明确拒绝，不虚报已执行强制。 |
| 08-P0-2 实现→测试→失败→修复→复验 | `provider.rs`、`provider/execution_tests.rs`、执行报告 / 命令证据；`execution_control_smoke.py` 14 组，故意失败命令与修复复验 | **已实现 / 模拟通过（Native）**。保留原失败、退出码、后续验证及停止原因；模型说“通过”不等于实际命令通过。不是所有模型/项目都会自动修好。 |
| 08-P0-3 待回答问题与授权 | `store/decisions.rs`、执行控制、`components/execution-requests.tsx`、CLI requests/decide；审批 digest、修订、到期、拒绝、撤销、重启回归 | **已实现 / 模拟通过**。问题回答与执行批准分离；原 task/action/revision 精确匹配，远程可答被授权问题但不能升级成 owner approval。未知副作用不自动重试。 |
| 08-P1-1a MCP 管理与实际调用 | `capabilities/{api,mcp,runtime,schema}.rs`、`store/capabilities.rs`、App / CLI 能力入口；`capabilities_smoke.py` 7 组、E08-C | **已实现 / 模拟通过：显式本机 stdio 子集**。注册默认禁用、显式 probe、目录 schema 有界、pin/revoke、调用实证与错误隔离；发现成功不是实际调用成功。未实现 HTTP MCP、自动发现或协议全部扩展；annotations 不授予权限，进程不是 OS 沙箱。 |
| 08-P1-1b Skills 管理 | 同上 Store、`components/capabilities-settings.tsx`；显式导入、旧任务固定正文、撤销、跨任务隔离 smoke | **已实现 / 模拟通过**。手动注册或明确 SKILL.md 导入；启用快照实际进入上下文。正文不自动加载脚本，不授予读写/外部权限；单项 / 合计正文有界。 |
| 08-P1-2 任务/角色权限、本机与外部写边界 | `execution_control.rs`、`project_files.rs`、MCP runtime、角色任务快照；路径/符号链接、预算/权限交集、外部写审批和 unknown 回归 | **已实现 / 模拟通过，有明确信任模型**。服务端权限不能由模型声明提升。`run_command` 需显式 `HARNESS_ALLOW_COMMANDS=1`，是受信 shell，**不是内核/网络隔离沙箱**；MCP 未知写入保留隔离/清理状态，不自动重放。 |
| 08-P1-3a 命名服务切换、主/角色选择 | `capabilities/{providers,provider_api,provider_runtime}.rs`、`store/providers.rs`、`components/providers-settings.tsx`；真实 owner HTTP + 本地 Native 模型测试、E08-P | **已实现 / 模拟通过**。任务/计划/协调任务固定 revision、endpoint、model 与私有 ref；改址/轮换使旧任务下次发送失败关闭，不静默换 host/key；models 同源，不继承 legacy API key/header。 |
| 08-P1-3b 系统凭据生命周期 | `capabilities/vault.rs`、`capabilities/vault/system.rs`、`store/providers.rs`；mock 成功/拒绝/队列/超时 unknown/CAS 竞态/重启/清理测试，E08-P | **代码已接线，真实 OS 授权待验**。Apple Keychain、Windows Local、Linux Secret Service；无明文文件/环境 fallback。单次意图、后台持有工作、确认后 CAS，未知不重试；ref 不公开、孤儿显式清理。平台编译通过不等于 OS 可用/解锁/拒绝路径已实测。 |
| 08-P1-3c MCP 专用环境凭据 | `capabilities/{credential_api,credentials}.rs`、`store/capability_credentials.rs`、MCP process；真实 owner HTTP + 内存 vault + stdio fixture，E08-M/E09/E14 | **已实现 / 模拟通过**。最多 8 个显式 slot，配置/cwd/env 策略绑定，修改配置先 detach；清空宿主环境，仅注入本服务器固定引用，不借 Provider key。新凭据与公开配置/目录冲突拒绝、秘密在模型/工具/差异采集前进入脱敏集合。 |
| 09-P0-1 一个实际 ChannelAdapter | `qq/{daemon,runtime,transport,events}.rs`、`store/qq.rs`；`qq_smoke.py` 的真实进程 + loopback HTTP/WS，E09 | **QQ 生产适配代码已接线 / 模拟通过，真实平台未验**。官方出站 WS、token、Identify/Resume、心跳及 C2C/群 @；没有真实账号鉴权/权限/消息送达证据，不能称“真实 QQ 已打通”。 |
| 09-P0-2 配对、授权、撤销 | `qq/{admin,policy,credentials,api}.rs`、Store QQ、App / CLI；陌生身份容量、配对确认、安全时间窗、时钟回拨/异常、撤权回归 | **已实现 / 模拟通过**。owner 逐次确认身份、对话和作用域，码不是控制授权；撤销后阻止未来控制/投递，不能召回在途消息。 |
| 09-P0-3 会话创建/选择/回复和显式编排 | `qq/commands.rs`、Store command transaction；旧任务路由、受限 DAG、阻塞模型中查询取消 smoke | **已实现 / 模拟通过**。身份/外部对话选择隔离；原 actor/route 固化，切换会话不改变旧任务回传；远端不是管理 API 代理。 |
| 09-P0-4 幂等输入、可靠 outbox | `qq/{persistence,notifications,transport}.rs`、`store/qq.rs`；重复事件、提交栅栏、ACK/Resume、429、EOF unknown、重启不重发 | **已实现 / 模拟通过**。DB 入站提交后推进检查点；明确 429 可按规则重试同 body/序号，未知结果不重发。保守被动回复窗口 ≤300 秒，到期单列 expired；不承诺恰好一次送达，无主动推送后备。 |
| 09-P1-1 卡片、摘要、授权产物 | `qq/presentation.rs`、`qq/notifications.rs`、ACL 结果查询；有界摘要/群结果授权/不自动带原日志回归 | **部分：有界文本降级已实现**。普通完成通知不附原始结果；授权查询可取有界摘要。**未做富卡片、附件/任意文件产物传输**，不能把文本 fallback 写成完整富媒体交付；是否继续扩展需按具体 QQ 权限验收。 |
| 10-P0-1 设备配对、出站认证、网关 | `crates/harness-gateway/src/{lib,directory}.rs`、daemon `gateway/*`、`store/gateway.rs`；`gateway_h5_smoke.py`、E10 | **已实现 / 模拟通过**。独立 gateway、daemon 出站 WS/WSS、owner 确认、cookie/device key/owner bearer 隔离；不向浏览器泄露本机 token。未做公网 TLS/nginx 部署。 |
| 10-P0-2 H5 会话、输入、进度、取消、问题 | `src/mobile/MobileApp.tsx`、`src/mobile/client.ts`、gateway 白名单 RPC；真实多进程 smoke、React/jsdom 与 client 测试 | **H5 授权模拟范围已完成**。同一后台事实、远端问题回答不等于执行审批；不会另建移动任务数据库。实际手机窄屏/触摸/WebView 视觉未验。 |
| 10-P0-3 离线、重连、重复提交 | gateway relay、daemon persistence、`src/mobile/client.ts`；断线/过期游标/降低 cursor/稳定 ID/未知回执/撤权/网关重启测试 | **已实现 / 模拟通过**。离线明确失败，无默认待执行队列；未知只查原回执、不换 ID 重发。browser session 仅内存，网关重启需重新配对；不是持久登录。真实移动网络切换未验。 |
| 10-P1-1 移动 diff / 产物 | `gateway/resources.rs`、MobileApp detail/ArtifactSummary、每次 ACL 复验；有界转义/资源越权/撤权回归 | **已实现的范围为有界纯文本**。结果/证据/观察 diff 通过 opaque resource ID，单份最多 64 KiB，明确截断；无任意工作区路径或二进制附件下载。视觉阅读体验仍待手机验证。 |
| 10-P1-2 多设备目录（按需） | gateway `directory.rs` 的登记/列举/撤销和上限 1,024；`MobileApp.tsx` PairForm | **后端基础已实现，H5 多设备切换界面未实现**。当前浏览器手填一个 device_id 配对；不声称多设备工作台。此项路线图本身为“确有需求时增加”，不擅自扩成跨设备编排承诺。 |

## 5. 1.0 稳定性与性能

范围及合同：[1.0 迭代](iteration-v1.0.md)、[v12 存储](protocol-v12.md)、[v13 索引](protocol-v13.md)、[v14 有界历史](protocol-v14.md)；实际测量与原始失败见[性能记录](performance-v1.0.md)。

| ID / 路线图要求 | 实际代码及可复现证据 | 当前判定 / 下一验收 |
| --- | --- | --- |
| 1.0-P0-1a 数据迁移与失败保护 | `reliability/storage.rs`、`store/reliability.rs`、`store/scheduling.rs`；旧版本真实 SQLite、v1/v2/v3 迁移、DDL 故障/回滚、未来版本拒绝，E12/E13/E14 | **已实现 / 模拟通过**。先数据目录锁/只读兼容检查再迁移；不重建较新或不一致 DB。还不是任意未来版本降级支持。 |
| 1.0-P0-1b 备份、离线恢复、恢复后撤权 | `reliability/{jobs,storage,api}.rs`、Store reliability、CLI maintenance、UI reliability；`reliability_smoke.py`、真实备份/inspect/恢复、E12/E14 | **已实现 / 模拟通过**。SQLite 一致快照、128 MiB 影像上限、一次性意图；离线 journal/锁/类型/摘要/完整性验证。恢复使任务、批准、续接及 Provider/MCP/QQ/gateway 权威失效，不自动重放/删除 OS 秘密。备份含私有历史，**不是脱敏导出**；SHA 检错不认证来源。 |
| 1.0-P0-1c 协议兼容 | 已发布 HTTP v1、v14 显式窗口 opt-in 与错误合同；core compatibility / 认证握手已随 v15 发布 | **已接线 / 模拟通过，最新 CI 尚未全绿**。旧无 query snapshot 维持全量语义或明确 413，不能静默给窗口；主动握手/不兼容启动拒绝的本地证据及精确提交结果分列第 6 节，不扩写为任意旧版本兼容。 |
| 1.0-P0-2 三平台安装、启动、退出、升级、取消、崩溃恢复 | 已发布 daemon 锁、任务中断恢复/取消/清理、跨平台构建 CI；actual lifecycle / startup / package fixture 已随 v15 / v16 发布 | **未整体完成**。v16 的真实 Linux HTTP/CLI/SIGINT/SIGTERM、observer 断连、paused 计划取消与硬崩溃隔离均已通过本地进程回归；开发分支已发布，但最新 Windows/macOS Rust CI 失败；失败后的阶段及三平台原生安装/升级/卸载须独立验收，不能由二进制临时复制代替。 |
| 1.0-P0-3 凭据保护、权限和脱敏 | vault / credential intent、执行权限、QQ/gateway ACL、`provider_redaction.rs`、`changes.rs`、诊断白名单；raw JSON key/value、跨块/跨轮/截断边界 canary 回归，E08–E14 | **已实现声明边界 / 模拟通过；OS 待验**。已知秘密不进入公开 snapshot/事件/错误/诊断；不可承诺识别任意未知密钥或任意编码。私有备份另行保护，真实日志/OS 权限实机检查仍需进行。 |
| 1.0-P0-4 发布校验、签名、更新策略 | 已发布 `reliability/update*.rs` / `store/updates.rs` / CLI updates 与 v15 协议 | **已接线并本地模拟通过；精确 CI 尚未全绿，生产发布条件未完成**。本轮仅本机 verification-only 校验/私有暂存，不自动下载/执行/安装。生产签名身份、信任根、更新源未配置，实际签名/自动安装/发布未授权且未做。 |
| 1.0-P1-1 诊断与用户文档 | `reliability/diagnostics.rs`、owner API、CLI maintenance、UI；固定字段/聚合、无私有值回归，README、协议、acceptance、handoff | **诊断已实现 / 模拟通过，文档需持续校准**。配置存在、连接成功、任务成功、投递成功分开。旧 iteration/status 的阶段性“未接入”与后续追加有时间差；以本矩阵证据 / 精确 SHA 为准，不靠旧标题推断。 |
| 1.0-P1-2a 长历史容量、活动项可见、完整只读恢复 | `store/history.rs` / `history_tests.rs`、`history_api.rs` / tests、CLI history、Native history、TS history-client / history-browser；E14、[protocol-v14](protocol-v14.md) | **有界容量已实际复测通过，不能声称无限容量**。窗口 12 MiB / 页 2 MiB（最多 80）/ 完整 task 8 MiB；超大整项 unavailable、keyset/固定上界、消息绝对 offset、owner selection/活动/pending 优先及遗漏计数。未载入不等于不存在，CLI/桌面不偷换目标，真实 wire 由 TS parser 验证。 |
| 1.0-P1-2b 性能与资源稳定性 | schema v3 索引、活动主任务 SQL 筛选、`scripts/history_benchmark.py`、[performance-v1.0](performance-v1.0.md) | **已测有范围；启动修复已发布，最新 CI 尚未全绿**。原 1,000/5,000 completed-only seed 保真，全分页与20k SSE实测通过；仅 Linux debug/shared cloud、小样本。缺 release/其他 OS 资源/活动任务压力/长期 soak/GUI 测量；不能外推普适 SLA。 |

### 大历史结果不能被总门禁数掩盖

- 原 1,000 / 5,000 seed 的 SHA 和逻辑内容未变，最终两组基准均 exit 0；使用独立固定 debug daemon/CLI，HTTP/CLI/SSE 预算为 **30 秒**。
- 窗口实际 **913,091 / 2,577,762 bytes**；完整任务 **13 / 63 页**、消息 **25 / 200 页**，核对全 ID、offset、原字段；离窗 status/follow 各组各 3/3；完整 20k SSE 各组各 3/3（序号/ID/计数 oracle，不声称逐字段验证全部 payload）。4 次备份/inspect、2 次独立恢复通过。
- 5,000 组 HWM **241,729,536 bytes**；温启动 **7,153.3 / 6,836.4 / 5,701.6 ms**，与当时 v14 Native **6 秒**等待冲突；后续 v15 已改为 30 秒单次启动观察，不覆写这些原始样本。诊断 P95 相较 v3 从 86.3 到 593.3 ms，不能宣传各项全面更快。
- 旧 no-query snapshot 六次明确 413 是兼容拒绝，不是把原 40 MiB 全量内容压成小响应。原 baseline/v3 失败样本和 runner 自身修正记录保留，不删掉失败后只展示 exit 0。
- 证据：`history-benchmark-v14-bounded/measured-final-cli/results.json`，SHA-256 `d98a208808888a94119cc4093ca12a7bc23223acfe48021ff27ad54048afc79c`；[性能记录](performance-v1.0.md)列出二进制 hash、归档路径、样本、旧候选及重跑命令。该测量来源是冻结 reviewed v14 候选，不能将它冒充发布后 CI 或另一组二进制性能。

## 6. v15 / v16 已发布：本地冻结通过，精确 CI 待修复

协议为 [v15](protocol-v15.md) / [v16](protocol-v16.md)。这两片已实际接线、独立审查并发布到开发分支，提交分别为 `7aea4f15` / `925e6cf2`，中间有 `43d12ee3` 窄 cfg 修复。以下表格保留原本地冻结证据，不是本次重新执行，也不能代替最新精确 CI。

| 工作项 | 当前实现 / 实测证据 | 明确边界 |
| --- | --- | --- |
| 认证兼容握手与启动 | Store / owner HTTP、CLI、Native bridge / 事件流、开发代理已接线；错误 generation 拒绝、512 bytes 响应边界、真实延迟子进程、不兼容不替换等回归通过。固定 v15 daemon 的 5,000 任务副本 readiness 4/4 在同一 30 秒预算内，最大 20.335 秒。 | 不是数据库加速、任意负载 SLA 或完整 GUI。原 seed 和全部历史未改；详细二进制/结果摘要见 [更新验收](acceptance-update-v1.0.md)。 |
| 校验 / 暂存与防回退 | 固定 owner trust、Ed25519 原字节清单、长度/hash/platform/schema/有效期、CAS、单次上传、真实 owned I/O、DB + 独立 witness、旧库恢复撤权均已接线；exact v15 core13 / daemon683 / CLI50 / gateway17、fmt/Clippy/build、更新与包布局各4组重验通过。 | verification-only；不下载、解压、执行或自动安装产物，不登记生产信任根。临时签名夹具不是生产签名身份。 |
| 同一实例实际停止 | 同 Store 锁 admission、真实 worker registry、OS 线程 join、进程清理、两次 quiesce、HTTP/CLI/UI 固定 UUID、observer 断连不取消 owner 已接线。v16 完整 core13 / daemon791 / CLI53 / gateway17、UI343、fmt/Clippy/build、Web/H5 构建、22个后台 smoke及Web代理4组通过。 | 正常关闭会取消运行/排队/暂停任务及计划，重启不自动恢复；硬崩溃仍保留 interrupted/unknown 和项目隔离。进程清理仅覆盖普通 Unix 进程组 / Windows Job 后代，故意 `setsid` 脱离不在保证内，不是恶意进程沙箱。原本地冻结时 Windows 10 个进程夹具仅交叉编译/Clippy；发布后已取得真实 Windows/macOS 测试失败，见下文。失败后的生命周期阶段及完整 Native GUI 不能算通过。 |
| 包布局夹具 | 只复制固定当前 daemon/CLI/gateway，实际认证使用、进程重启、保留历史与仅移除副本；新 freshness 自测保持真实 library/manifest 依赖，CLI 不错误依赖 daemon binary main。 | 不是三平台原生安装器、生产签名或真实升级/卸载。Windows 精确 CI 已跑到 package 输出编码失败；新修复尚无提交/CI。 |

v15 原文档冻结候选 tree 为 `472e5969210a0afbe60186ab046b6081b5e7cd9d`（历史本地对象，不是当前 HEAD）；其测量源码 tree `105a702e4c9a5a42b756bf22f77e3943704f3ce7` 后只更新验收文字。v16 本地源码/固定二进制及原失败保留记录见工作区 `drain-frozen-candidate.json` 和 [生命周期验收](acceptance-lifecycle-v1.0.md)。这些测试数量存在包含关系，不能累加或写成 E14 已发布门禁。

精确 CI 进展与原失败保留：

- v15 `7aea4f15` 的 push `40827179414` / PR `40827184463` 均终态 **5/6**，仅在 Windows Rust Clippy 遇到 `unused_mut`；该失败后的阶段未运行。`43d12ee3` 仅把目录 builder 可变绑定放入 Unix cfg，不改变生产目录/权限逻辑；其 push `40828345766` / PR `40828352198` 均终态 **5/6**，macOS / Ubuntu Rust 全部 smoke 与三平台 Desktop 成功；Windows 已越过产品 Rust 测试、构建和 update 4 组 smoke，仅最终 package 中文输出被 cp1252 stdout 拒绝，完整 CI 仍非通过。
- v16 `925e6cf2` 的两轮 CI 均终态 **4/6**：三平台 Desktop 和 Ubuntu Rust 通过；macOS daemon **790/791**，僵尸进程组信号探测夹具失败；Windows daemon **728/730**，两个新 CMD / Job 复合脚本夹具失败。对应修复正在工作树中进行，**未提交、没有新精确 CI**。
- 原 GitHub 内容上传确曾被 auto-review 要求额外披露确认，后续用户已明确授权 v15/v16 与 QQ 相关代码开发分支发布；此阻塞已解除，不再等待用户确认。授权没有扩大到真实 QQ 消息、OS vault、部署、生产签名或正式发行。

## 7. 真实环境验证清单（与代码缺口分开）

| 尚未运行的验证 | 本轮已有替代证据 | 后续所需条件；不作误导性结论 |
| --- | --- | --- |
| 真实 Native / Codex 模型、Codex 登录和供应商计费行为 | 本地 HTTP/stdio fixture；官方 CLI schema / 身份合同核查 | 用户本轮明确只做模拟；无真实调用。若后续需要，应另定服务、凭据与费用边界；不是用“缺凭据”掩盖可模拟实现缺口。 |
| Apple / Windows / Linux 真实 vault 的写读删、锁定、拒绝、超时 | target-specific 编译、mock serial worker、真实 owner HTTP 的内存 backend | 在各目标 OS 明确授权的测试账户/专用条目验证，不能因 CI 编译成功便声称系统凭据工作。 |
| 真实 QQ 官方鉴权、机器人权限、C2C/群对话、限流/回复时效 | 实际 daemon + 本地 HTTP/WS + Store/CLI smoke | 需要官方测试机器人、限定身份/对话与真实发送授权；没有 enrollment / 真实消息。300 秒是保守应用策略，不是实测平台保证。 |
| H5 公网 WSS/TLS/nginx、跨网断连与手机视觉 | 本地 daemon/gateway 多进程、jsdom、Web/mobile 构建 | 用户自行通过 nginx 部署；已有 `deploy/nginx-gateway.conf.example`，当前未运行 `nginx -t` 或部署。真实手机/移动网络/窄屏截图待验，不能把浏览器沙箱失败记成通过。 |
| 三平台完整 GUI、明暗/缩放、安装/退出/升级/卸载 | 历史三平台 Rust/Desktop CI、Native 路由模块提取、fixture | 目标平台实际应用/安装环境与可复现过程；构建、桥测试、临时复制二进制不能替代。当前没有完整 GUI 成功声明。 |
| 生产签名、更新源、自动安装、tag/release | v15 仅本地临时签名 key / verification-only 设计与测试 | 用户未授权生产签名/自动安装/发布，身份和基础设施未指定；不自行采购/登记/发布。用户本地审核合并不等于授权这些动作。 |
| release 性能、其他 OS 内存/启动、活动队列并发与长期稳定性 | Linux debug 的原 seed 保真性能与真实回放/恢复 | 后续有控制变量、固定二进制/数据、真实预算与样本的测量；这些是待执行工程验证，不应统称外部凭据授权阻塞。 |

## 8. 更新本矩阵的规则

1. 每次新增“通过”必须带精确 SHA / 固定二进制身份、实际 gate 与范围；CI 失败、未运行和不适用分开。发布新提交后保留旧失败及修复归因，不覆写成从未失败。
2. 优先关闭当前待办：当前跨平台窄修复的独立验证、提交与精确 SHA CI，以及三平台实际生命周期/安装验收。实际 lifecycle/兼容、5,000 历史 readiness 和 verification-only 的本地证据已列第 6 节，不回写成最新 CI 成功或真实外部验证。真实平台未验与可开发缺口不能互相替代。
3. 独立记录产品取舍：managed ChatGPT 安全 fresh fallback、Codex 不支持的强制预算、QQ 文本降级、H5 纯文本产物/单设备配对，以及明确排除的 0.11。若后来扩展，补协议和验收，不悄悄改“已完成”的含义。
4. 最终交付至少提供开发分支/提交、各 gate 通过/失败/未运行、此矩阵剩余项及用户本地审核步骤；**只有代码和授权模拟范围的完成，不自动意味着 1.0 生产可发布**。

### 本次 CI 窄修复本地重验（2026-10-29 06:46 UTC）

CMD 固定脚本编码、macOS WNOWAIT 观察夹具及 package UTF-8 输出修复已独立复核。重新完整通过 Rust **13 / 791 / 53 / 17**、fmt / all-target Clippy / 后台构建、**22 个 smoke**、history self-test 和 package **6 项** self-test；新 Windows 13 项仍仅交叉编译，真实跨平台结果等新提交 CI。原失败保留，详细证据与未运行项见 [生命周期验收](acceptance-lifecycle-v1.0.md#本次跨平台-ci-修复的重新冻结2026-10-29-0646-utc)。本地通过不等于正式发行或实机验收。

### Windows 路径兼容性跟进（2026-10-29 07:07 UTC）

后续已发布 `df065790` 两轮 CI 各 **5/6**：macOS / Ubuntu Rust 与全部 Desktop 通过；Windows **732/733**，只有新增 PowerShell 初始化/超时测试失败，后续 smoke 未运行。本次进一步把已锁定 handle 的 canonical image/cwd 转成严格等价普通 DOS 表示；所有锁、Job、权限和 10 秒回归截止不变，无自动重试。Windows image/cwd 当前仅支持 UTF-8 可表示、少于 260 个 UTF-16 code unit 的普通固定盘符路径，歧义或不支持的命名空间明确拒绝；不是通用长路径支持。Linux 52/47 定向、Clippy 与 Windows 14 项交叉编译通过，独立复核 clear；实际新 Windows A/B/完整 CI 未取得，不提前称根因已证实或平台全绿。详见 [生命周期验收](acceptance-lifecycle-v1.0.md)。
