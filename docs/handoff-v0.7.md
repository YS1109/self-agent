# Self Agent 0.7 开发交接

更新日期：2026-10-28。交接性质：0.6 基线与 0.7 分切片开发记录；事件消费、执行器续接及补充/继续/区间改动切片已实现，未发布完整 0.7。

## 接手先读

1. 根目录 [AGENTS.md](../AGENTS.md)：项目规则及当前协议入口。
2. [分版路线图](roadmap.md)：0.7 至 1.0 的范围、依赖、验收与待定事项。
3. [0.7 迭代计划](iteration-v0.7.md)：下一阶段的实现顺序及验收条件。
4. [当前实施状态](implementation-status.md)、[本分支协议](protocol-v6.md) 与 [验收跟踪](acceptance-v0.7.md)：实际支持能力；[0.6 协议](protocol-v5.md) 保留为基线。
5. [架构计划](architecture-plan.md)：长期边界；未实现部分须与当前协议区分。

用户要求把规划和交接提交 Git，再委托另一个 Agent 开发。原始交接提交不包含 0.7 实现；用户随后已授权按切片开发，推送 codex/ 分支并创建草稿 PR，最终由用户本地审核合并。不得自行合并、打发布标签或公开发布。不要将路线图当作自动部署、发通讯消息或提升权限的授权。

## 不应再偏离的产品方向

- 日常体验参考 Codex：主 Agent 直接读取项目、修改、验证和回复，无需先建计划草稿。
- 子 Agent 编排与流程图主要用于显式远程编排；保留现有能力，但不作为桌面普通对话入口。
- 桌面、纯 HTTP 和 CLI 必须共用 Rust 后台；后续通讯和移动端继续复用相同会话。
- 全部产品文案默认简体中文。保持中性灰和蓝色，不使用绿色作为成功色、连线或图标颜色。
- 显示缩放已实现，不再以整体增减固定字号代替用户设置。
- 子 Agent 的模型及上下文策略可以独立覆盖；任务快照不能被之后的设置修改覆盖。

## 当前功能基线

仓库：`YS1109/self-agent`，公开项目，远端默认工作目标为 `main`。

0.6.0 功能提交：`764588561e50caab9c7f0b7b137d29db191ae94d`。该 SHA 是功能基线，后续规划文档提交不修改运行版本。接手应获取最新 `origin/main` 并先检查工作区，保留已有用户改动；需要新分支时沿用 `codex/` 前缀。

| 能力 | 0.6.0 实际状态 |
| --- | --- |
| 日常主任务 | `run_agent` 直接执行，`task_kind: "main"`；同会话按 `main_sequence` 排队 |
| 历史 | 启动时读取前序对话及真实结果，不带入后续排队输入 |
| 执行器 | Native Chat Completions 工具循环及 Codex app-server stdio |
| 会话续接 | 应用历史续接已实现；每个 Codex 任务仍新建临时 thread |
| 流式与事件 | Native 非流式，Codex 仅采集部分协议通知；SSE 实时广播，桌面轮询快照，无游标历史补发 |
| 上下文 | 模型默认 128000 / 80%，角色逐项覆盖；Native 实际压缩但摘要只用于当前执行 |
| 模型发现 | 自定义服务和同源模型列表 Endpoint，实际拉取及下拉；不跟随重定向 |
| 显示 | 80% 至 120%，默认 95%，本机保留；灰蓝明暗主题和中文菜单 |
| 无头 | 独立 daemon、HTTP、`self-agent-cli`；仅监听 `127.0.0.1` |
| 远程 | 模拟入口与本地持久化回调；尚无真实通讯、远程配对或公网网关 |

配置来源可复用本机 Codex 顶层自定义服务，也可手动配置。凭据不得输出、提交或复制到前端。模型列表通过、版本检测通过和配置完整，都不等于实际模型调用成功。

## 已完成的验证

以下是 0.6.0 的历史验收结果，不是本次文档变更重新跑出的测试：

- 共 172 项测试：前端 68、core 5、daemon 95、CLI 2、desktop 2。
- 格式、Clippy 与八组 HTTP / CLI 回归通过。
- GitHub Actions `40646566229` 对应上述功能 SHA，三平台 Rust / 协议和桌面共六项成功。
- 隔离项目的真实 Native 文件读取、切换 Codex 后使用同会话前序结果通过；上下文及压缩参数透传已验证。
- macOS 应用与 DMG 打包安装完成；原生 GUI 验证缩放、下拉、上下文与角色覆盖保存及重启恢复，原会话和预设保留。

尚未证明：真实 Codex 长上下文自动压缩、thread 原生恢复、远程平台实际投递及其他设备的 GUI 行为。默认 CI 使用本地 HTTP / stdio 夹具，不消耗云端额度；Codex 可执行夹具目前仅 Unix 运行，Windows 的角色 HTTP 回归使用 Native。

## 源码入口

| 路径 | 接手时重点 |
| --- | --- |
| `crates/harness-core/src/lib.rs` | 共享任务、消息、事件、指令及收据类型 |
| `crates/harness-daemon/src/lib.rs` | HTTP 路由、SSE 广播、worker、直接主任务执行和终态 |
| `crates/harness-daemon/src/store.rs` | SQLite、幂等指令、主任务顺序、历史、计划、回调与重启恢复 |
| `crates/harness-daemon/src/codex.rs` | stdio 生命周期、thread / turn、通知解析、中断与进程树回收 |
| `crates/harness-daemon/src/provider.rs` | Native 流式工具循环、持久摘要边界与取消 |
| `crates/harness-daemon/src/context.rs` | token 估算、保留工具组、压缩边界和历史拼接 |
| `crates/harness-daemon/src/coordinator.rs` | 远程规划和回顾；不要把它重新设为桌面直接主任务必经入口 |
| `crates/harness-daemon/src/configuration.rs`、`model_config.rs`、`profiles.rs` | 模型来源、检测、上下文继承与角色快照 |
| `crates/harness-cli/src/main.rs` | 本机鉴权、JSON 命令、轮询等待及退出码 |
| `src/App.tsx`、`src/api.ts`、`src/types.ts` | 快照与事件消费、聊天、发送入口及前端协议 |
| `src-tauri/src/bridge.rs` | 后台启动与版本检查、受控请求；event_stream.rs 提供私有 SSE/Channel 桥 |
| `src/components/execution-evidence.tsx`、`src/styles.css` | 执行证据和现有视觉语言，可在此基础上接入过程及更改展示 |
| `scripts/codex_fixture.py`、`scripts/headless_smoke.py` | 协议和会话历史夹具，适合扩展恢复与流式回归 |
| `.github/workflows/ci.yml` | 三平台后台、桌面、原八组与新增流式/续接/代理 smoke |

新增模块可以从现有大文件拆分；先明确接口边界，避免与行为修改混在一次大规模重写中。

## 当前开发分支进展

- 分支：`codex/v0.7-native-events`，从干净的 `5b1e541798d99a4c731b6892b6a30bf9dc9f121b` 起步，未覆盖其他设备的用户修改。
- 已完成 Native 直接任务的回复增量、工具始末、用量/压缩事件、持久日志、SSE 断线回放与 CLI 跟随；新协议为 `protocol-v6.md`。
- 旧事件迁移、终态事务、迟到输出、保留窗口、跨分片脱敏和工具禁止重放都有回归；新增 `scripts/streaming_smoke.py`。
- 第二切片新增 Codex 事件/主 thread 持久续接、Native 跨任务摘要及桌面/浏览器流消费；配置指纹、历史边界、未知写入和有界 ACK 均有回归。当时尚未包含上下文估算展示和 P1；最新进展见下方第三切片记录，GUI 仍未验证。
- 首切片发布 SHA `bb0b7e08fa2d94f498c9b428206f8aebcab05273`，草稿 PR #1；push CI `40721056695` / PR CI `40721141000` 均六项成功。后续 SHA 不沿用该结果。
- 首切片本机验证：core 5、daemon 113、CLI 15、前端 68 项测试通过；原八组 smoke、新增八组流式场景、格式、Clippy、后台/CLI 与 Web 构建通过。用户限定模拟模型；真实模型、桌面 GUI 未测，三平台 CI 按远端提交另记，不复制旧版记录。

第二切片本机冻结验证：core 5、daemon 148、CLI 15、前端 87、Rust 事件桥 7 项通过；原八组回归、流式八组、续接八组和 HTTP 代理三组场景通过；格式、全目标 Clippy、后台/CLI/Web 构建通过。仅模拟模型；本机 Tauri 系统库缺失、Chromium socket 沙箱阻止 GUI 验收，未绕过限制。ChatGPT 身份不能可靠绑定时目前明确新 thread 导入历史，原生 thread 续接限已验证身份的自定义服务；仍属于待完善的 P0 兼容边界，不能称完整 0.7 已完成。提交后按远端 SHA 追踪三平台 CI。

第三切片工作树已接入补充/继续、持久估算和区间改动。协议见 `protocol-v6.md`，逐项验证见 `acceptance-v0.7.md`。第二切片发布为 c013d740，跨平台测试修复为 b97d1f71；后者两次 CI（40736034504 / 40736044231）各六项成功。下一切片不能沿用这些结果。该切片结束时的剩余项是普通托管登录 thread 复用、安全 Git 概览和 GUI 视觉验收；Git 后续进展见下文。不要为完成列表重新打开不受控 git status 或读取认证文件。

第四切片工作树已增加安全 Git 元数据读取：不运行 Git 子进程，用有界 no-follow 解析 index、refs 和 loose/packed HEAD 树，暂存比较与工作区原始字节分开，转换/忽略语义明确未知。独立复审已关闭损坏 refs/树及遗漏路径的错误推断。隔离 core 5 / daemon 195 / CLI 19 / 前端 105、格式、Clippy、构建和十二个后台 smoke 均通过；跨平台 CI 待对应提交。完整限制与冻结验证见 protocol-v6 / acceptance-v0.7，不能将 partial 概览称作全部 Git 状态。P1 跨平台修复 `7142474c` 的 push 40746966975 / PR 40746977940 两次各六项成功。

第五切片保留安全托管登录回退，并补齐可取消有界探测、启动前根路径校验、失败报告的提前私有脱敏及隐式环境身份指纹。公开协议不含不可变 principal，不能把实验工作区 ID 当作用户身份；证据见验收记录。旧接口兼容，执行预算接线在后续 0.8 切片。隔离 core 5、daemon 203、CLI 19、格式、Clippy、后台构建及十二个后台 smoke 通过；未修改/重跑前端和 GUI，精确提交 CI 另记。

## 0.7 第一个开发切片

先完成统一事件的协议与持久化设计，再打通一个 Native 直接任务的“回复增量、工具执行、终态、断线恢复、CLI 跟随”，之后接入 Codex 和桌面。并行推进会话恢复前，先确定事件 ID、顺序及执行器会话关联的共享类型，避免多人改同一文件。

托管账户原生续接已核实受公开协议缺少不可变用户标识阻塞，保留安全 fresh-history 回退，不读取认证文件或降低身份要求。其余后续切片继续推进；安全 Git 概览已是有界 partial 范围，完整转换/忽略语义不在该读取器承诺内。具体要求及未验证边界见迭代计划和验收跟踪。0.8 以后顺序见路线图，QQ/H5/用户自部署已确认；签名与公开发布仍未授权。

Codex 的恢复、注入和通知协议必须依据实施时安装版本的 schema 或官方文档核实，先用本地夹具验证；真实模型任务仅在另获授权后独立验证。不要仅因某个方法名看似合理就写死协议。

## 开发与验证入口

依赖要求见 README；当前 Node.js 24、Rust 1.96 或更新版本，pnpm 版本由 `package.json` 指定。

```sh
pnpm install --frozen-lockfile
cargo fmt --all -- --check
cargo test -p harness-core -p harness-daemon -p harness-cli
cargo clippy -p harness-core -p harness-daemon -p harness-cli --all-targets -- -D warnings
cargo build -p harness-daemon -p harness-cli
pnpm test
pnpm build:web
```

```sh
python3 scripts/smoke.py
python3 scripts/provider_smoke.py
python3 scripts/configuration_smoke.py
python3 scripts/orchestration_smoke.py
python3 scripts/agents_smoke.py
python3 scripts/runtime_smoke.py
python3 scripts/plan_change_smoke.py
python3 scripts/headless_smoke.py
python3 scripts/streaming_smoke.py
python3 scripts/continuation_smoke.py
python3 scripts/steering_smoke.py
python3 scripts/changes_smoke.py
python3 scripts/web_stream_smoke.py
```

按实际改动增加有行为价值的回归；桌面构建入口为 `pnpm desktop:build`。完整门禁与平台依赖以 CI 和 README 为准。真实模型选项单独运行，记录消耗及验证范围，不混入默认测试。

第三切片冻结本地门禁已通过：core 5、daemon 185、CLI 19、前端 104、Rust 事件桥无头 7 项，格式、Clippy、后台/CLI/Web 构建及十二个后台 smoke 与 HTTP 代理三组。对应提交 CI 另行跟踪；GUI 与真实模型未验证，详见 [验收跟踪](acceptance-v0.7.md)。

## 数据、运行环境与边界

- 开发、协议夹具和真实模型验证使用独立 `HARNESS_DATA_DIR`、本机临时端口和隔离项目，不能向用户原会话灌入测试任务。
- 日常数据位于平台应用数据目录的 `self-agent` 下。元数据 `daemon.json` 含本机令牌，不能提交、输出或交给浏览器。
- 本机 `.local/` 中有上一轮验证和恢复资料，已忽略，不属于 Git 交接内容；接手不能假设其他机器存在这些文件。
- 0.6 后台重启会将执行中的项目任务标为失败并保留改动，不自动重放写入；0.7 的会话恢复不得破坏这一边界。
- Codex 当前拒绝未支持的交互授权；不要为了跑通示例取消权限约束。Native 命令执行由现有显式配置启用。
- 文档提交不需要重装应用或停止日常后台。实现升级涉及桌面与 daemon 版本一致性，届时再按具体交付处理。

## 接手交付要求

提交前更新实际实现状态、协议、README 和本交接记录，区分已交付与剩余目标。给出代码提交、对应 CI 和有意义的实测范围；失败或未验证的部分如实保留，不能直接复制上一版的“全部通过”。

用户现已授权按切片推进 0.7–1.0：QQ 机器人为首个通讯入口，用户自行通过 nginx 部署网关，本轮先完成 H5、不做独立 App；仅模拟模型测试，允许官方依赖安装及开发分支/草稿 PR/CI。仍不自动部署、发送真实平台测试消息或公开发布。每切片独立验证与提交。


## 0.8 命名服务交接

命名 Native 服务与系统 vault 生命周期已接入 Rust、桌面桥、Web 和 CLI，见 protocol-v9 / acceptance-providers-v0.8。任务及计划角色绑定具体服务修订；后续节点不得因当前配置轮换而静默换服务，撤权导致阻塞时保留已完成前驱。规划结果提交事务再次检查服务身份。系统凭据未知操作不重放，cleanup_status 与原操作状态独立保存。

托管 MCP / Skills 的平台修复最终提交 `7cd1b16c50a75985e4e7428a956da99c33ead0c0` 已通过 push `40766271204` / PR `40766281131`，两轮各六项三平台检查成功；此结果不代替命名服务切片 CI。后续接入 MCP 环境凭据及 QQ 专用 credential scope，不能使用 Provider 密钥替代外部工具/机器人授权。真实系统凭据、GUI、真实模型均未运行。

命名服务候选冻结门禁通过：core 7 / daemon 322 / CLI 27 / 前端 166，事件桥 7、Web 代理 3 组、15 个后台 smoke、fmt / Clippy / 构建；独立审查闭环。下一步先核验此切片精确提交 CI，再继续 MCP 环境凭据与 QQ 专用 scope，保留未验证实机系统存储和 GUI 边界。


MCP 环境凭据后续切片见 protocol-v9 附节与 acceptance-mcp-credentials-v0.8：独立 slots/ref、固定 task / probe scope、一次性 vault 操作和实际进程 env；不能在设置中手填 refs，也不能复用 Provider 凭据。秘密 JSON 文本需语义处理，匹配模式每次复用而非逐字段重新编译。命名服务 `675c326d` 精确 CI 两轮各6/6已成功，MCP env 的新提交仍独立跟踪。QQ 共享接线工作树与 1.0 新 reliability 模块由各自负责人持有，不能把未验收内容混入本切片提交。

## 1.0 存储维护交接（第一应用切片）

真实 Store 启动与离线恢复统一经过数据目录独占锁、只读版本检查及 schema ledger；不再把 guard 检查后的路径字符串当作目录身份保证。备份意图先持久化，再由独立 worker 导出一致 SQLite 影像，HTTP 断连后只读查询 UUID。恢复只离线、必须显式确认摘要，并撤销所有旧执行/远端/凭据操作权威；原始历史结果不改写成成功或重放。

后续 gateway schema 必须明确提高 ledger 版本并加事务恢复撤权 helper，不能只往当前 schema 1 偷添表。生命周期和更新独立模块暂未接线；不能在提交中混入并冒充已验收。见 protocol-v12 / acceptance-v1.0；当前仍未运行真实 GUI、真实模型/QQ/系统凭据或实际安装发布。

本切片独立候选冻结门禁通过：core 7 / daemon 527 / CLI 35 / 前端 222，事件桥 7、维护路由提取 1、Web 代理 3 组、17 个后台 smoke，以及 fmt / Clippy / 构建。独立源码复审闭环；精确 SHA CI 另行跟踪，真实 GUI/系统凭据/外部服务/安装发布仍未验证。

## 0.10 网关/H5 后续接手

0.10 接线与边界见 `protocol-v11.md` / `acceptance-v0.10.md`。当前 gateway wire 在 core，独立服务在 `crates/harness-gateway`，daemon private modules 在 `gateway/`、可信 Store 边界在 `store/gateway.rs`，移动构建在 `src/mobile`。schema 已显式扩到 v2；后续表结构变更不能绕过迁移 ledger 或恢复撤权。

真实本地 HTTP/WS 联合入口是 `scripts/gateway_smoke.py`；它只用临时设备 token、模拟 Native HTTP 和 isolated SQLite。不要把其通过扩写为真实公网/TLS、系统凭据、原生 GUI 或手机浏览器视觉验收。源码冻结、提交与 CI 结果按 0.10 验收文档更新。

## 1.0 性能第一切片：索引调度与保留失败

显式 schema v3 索引调度已接入 Store，旧 v1/v2 的启动与备份恢复依次迁移；候选筛选不再反序列化全部已完成历史，仍保留暂停/串行/远端撤权语义。协议见 [v13](protocol-v13.md)，实际同 seed 指标见 [performance-v1.0](performance-v1.0.md)。

1,000 / 5,000 任务的空闲 CPU 和完整 SSE 回放已有明显改善，但 5,000 任务全量快照仍超出 CLI 16 MiB 上限，高峰内存与活动主任务历史检查仍待后续有界读取切片。失败保留，不宣称性能验收全部通过。生命周期及更新 helper 不混入本提交；实际门禁和精确 CI 见 acceptance-v1.0。

## 1.0 有界历史读取切片

真实 Store / owner HTTP / CLI / 桌面桥 / Web 已接入显式 `snapshot?window=1`（12 MiB）、固定上界历史分页（2 MiB）和完整单任务读取（8 MiB），详见 [protocol-v14](protocol-v14.md)。旧无 query 快照仍保持完整集合语义，超预算明确失败，不把旧客户端悄悄切成局部数据。窗口和审计遗漏、超大记录与未知状态均有 metadata；会话消息使用绝对位置。

历史页首面也保存 `current_cursor`，返回/刷新不混入新记录；实例重启使旧游标失效，不重放命令。CLI status/follow 使用原任务 ID，不下载全历史；桌面固定目标离窗时保留 ID/草稿并阻止转投，QQ/H5 本机授权表单不允许隐藏会话被提交。活动主任务前序检查只读取活动状态，保留精确 u64 顺序。

本片不包含尚未接线的生命周期、更新暂存或兼容握手。大历史同 seed 的实际结果、冻结门禁和精确 CI 分别记录于 performance-v1.0 / acceptance-v1.0；不能用页面单测或历史 CI 冒充容量、完整 GUI 或安装验证。

## 1.0 校验/暂存与兼容接手

协议v15/schema v4 的更新代码位于 `reliability/update*.rs`，实际同Store适配器在 `store/updates.rs`；floor与数据目录下独立pinned witness共同保留下限，恢复不得删除/降低witness。只允许本机owner配置verification-only信任，生产下载/安装不开放。CLI updates可实际暂存惰性字节，UI仅观察。

compatibility由core唯一wire类型驱动；Native `startup.rs` 保留单次进程尝试、有界metadata读取和revision状态，不能在不兼容/超时后自动替换后台。生命周期后续必须注册真正上传/metadata blocking worker并与Store admission同锁闭合，不能把HTTP observer drop当settled。本切片没有宣称drain已交付；验收和未运行项见 `acceptance-update-v1.0.md`。

## 1.0 实际 drain 接手

新增合同在 `protocol-v16.md`；核心是 AppState 的 `LifecycleRuntime`、同 Store 锁 admission、持续 worker monitor、真实 JoinHandle 所有权和首次/末次 quiesce。`admit_store` 只允许同步 SQL/校验；返回错误表示执行结束，不证明提交未发生，也不允许在闭包内启动未登记外部工作。

HTTP 关闭必须带当前 `expected_instance`，CLI/UI 只发送一次并保留原实例观察。Vault 需真实 OS 线程 join；进程组/Job、阻塞 I/O、上传/备份和远端在途工作均不能因 observer 被丢弃而记为 Stopped。物理 Unknown 保持租约；历史 outcome_unknown 不被清理成功覆盖。

接手前按 `acceptance-lifecycle-v1.0.md` 核对最新源码、固定二进制门禁、精确 SHA CI 和仍未运行项。不要把开发工作树、Linux/交叉编译、headless 夹具分别冒充已发布、Windows 执行或完整 GUI/安装验收。
