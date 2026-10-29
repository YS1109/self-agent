# 1.0 实际后台生命周期验收记录

状态（2026-10-29 06:34 UTC）：v16 已推送开发分支，精确提交 `925e6cf2d924141a0cff972113d5b37556891de9`（tree `29fb0f4b0ce4c42cada299f773a5ad1e5ff5b251`）。两轮精确 CI 均终态 **4/6**，macOS / Windows Rust 尚未通过，窄修复已完成独立复核和本地完整门禁，随本文提交、仍待新精确 CI；不能写成全绿或 1.0 正式发行。下列本地冻结、独立审查和模拟门禁是此前保留的证据，不是本次重新运行或新 CI 结果。

## 已取得的定向证据

- lifecycle runtime/control/Store 当前过滤 **76/76** 通过，包含实例绑定、真实 worker registry、两次 quiesce、observer 断连、tracked settings 读取、启动失败真实清理与 SQL 已提交后 Err 不误称回滚。
- 真实 owner HTTP / worker 定向：命名 Provider **7/7**、MCP 凭据 **6/6**、vault **18/18**、MCP stderr 真实析构等待 **1/1**、backup API **10/10**、history API **4/4**。覆盖 vault backend Drop 阻塞不得误报停止、关闭后新意图拒绝、配置/历史读取 observer 断连仍等待原 worker。
- CLI 生命周期 **3/3**：精确响应字段/一致性、显式确认、实际 TCP 关闭响应丢失仅一次 POST。后续实际 daemon+CLI 成功流程另由 smoke 验证，不以 TCP 夹具代替。
- QQ **126**、Gateway **63**、Update runtime **17** / Store **5** / API **5** 定向通过；原有发送结果与当前物理清理分开。
- Native bridge 实际源提取 **27** + 专用子进程入口 **1** 通过；已观察 peer 离线不会自动启动替代后台，关闭观察的 HTTP 等待限 35 秒。不是完整 Tauri/GUI。
- UI controller/component/proxy 定向 **27**、完整前端 **343/343**、TypeScript 与 Web/mobile 构建已通过；这是本地冻结证据，不是远端 CI 或完整 GUI 结论；最终同一源码的固定二进制回归见下表。

## 完整本地冻结（SIGTERM 跟进前）

398 个源码文件与门禁前 SHA 清单一致；core **13** / daemon **791** / CLI **53** / gateway **17**、全格式与 all-target Clippy、daemon/CLI/gateway 和 update fixture 构建通过。源码/日志/二进制摘要记录于 `drain-pre-sigterm-gates/receipt.json`。这批尚未包含随后明确要求的 Unix SIGTERM 优雅停止；不会将它冒充该跟进修改或最终发布 CI 通过。

## SIGTERM 跟进后的本地冻结

同一 supervisor 已接受 Unix SIGINT/SIGTERM，信号流保留至清理结束；真实关闭 **5 组**、完整续接 **11 组**、执行控制 **14 组** 通过。明确硬崩溃场景改用 kill，原 failed/interrupted/no-replay 断言保留，并新增 cleanup unknown 的同项目拒绝证明；独立预算用新临时项目，不清除旧隔离。首次旧夹具在隔离项目继续执行失败的日志保留。

随后再次完整运行：core **13** / daemon **791** / CLI **53** / gateway **17**、fmt / all-target Clippy / 全后台构建和 update fixture 通过。SIGTERM 与窄夹具 delta 已独立只读复核；以下回归及外部验收仍分开记录。

## 最终固定候选门禁

所有生产源码已独立复审；最后只是明确关闭影响的 UI/CLI 文字和窄夹具修正，随后再次完整 Rust 门禁与固定二进制回归。

| 检查 | 结果 |
| --- | --- |
| 完整 Rust | core **13** / daemon **791** / CLI **53** / gateway **17**（3 unit + 14 loopback），全部通过 |
| fmt / all-target Clippy / 构建 | 通过；daemon / CLI / gateway / update fixture 均从本次源码构建 |
| 前端与构建 | 完整 **343/343**、TypeScript、Web / H5 构建通过；不是实际 GUI |
| 固定私有二进制后台回归 | **22 个 smoke 脚本**全部 exit 0；另 history benchmark self-test 与 package freshness/删除边界 **5 项** self-test 通过 |
| 实际后台停止 | **6 组**：认证/错误实例/错误协议拒绝、HTTP observer 断连、真实 CLI 单次关闭、paused 计划 SIGTERM 取消及重启拒绝 resume、SIGINT 与 SIGTERM 活跃任务取消/退出/重启不重放 |
| Web 流式代理 | **4 组**通过：真实中文 SSE/断连、Origin/参数边界、不兼容 peer 不转发、游标冲突/私有 token 轮换 |
| Native bridge | 已核同一源的 **27 项 + 1 专用子进程入口**；完整 Tauri/GUI 与目标 OS 执行另列 |
| 进程清理专项 | Linux 实际 helper **7/7**；Windows **10 个夹具仅交叉编译与 Clippy**，没有 Windows 执行声明 |

进程所有权保证仅覆盖所启动的普通 Unix 进程组 / Windows Job 后代，不是恶意进程沙箱；Unix 故意 `setsid` 脱离不在保证内。清理 Unknown 保持未确认状态与租约，不由任务终态推断物理停止。

私有冻结候选 `self-agent-drain-final/`、398 文件和二进制清单 `drain-frozen-candidate.json` 位于执行工作区，归档及二进制本身未上传 GitHub；相关源码已随上面的 v16 提交发布。以下仍是原本地冻结二进制 SHA-256，不是 CI 产物摘要：

- daemon：`d5561661d0aa124bed2f5102b1970aa3a2c4ff6e171db8b859e8470085814bde`
- CLI：`7dc865287420b3a22564e5ec1aecffaec89eae1f05fe3d468a4dda8cbab30c5b`
- gateway：`056c252c7e3794664b8ab1c0c77205bf7ad6e00474373bb995f0f941e2eb1b63`
- update fixture：`49ad293cebdbafa063791ce78e67dac97034b453532f5a94176efe0fdc01ea61`

最终日志：`/tmp/drain-final-{fmt,rust,clippy,build}.log`、`drain-final-smokes3.log`、`drain-shutdown-warning-ui.log`、`drain-shutdown-warning-tsc.log`、`drain-web-build-final.log`、`drain-mobile-build-final.log`、`drain-web-stream-final.log`。不同定向测试包含于全量门禁，不将测试数相加。

### 已保留的失败与修正

- 审查发现的启动失败未 join、未登记配置读取、stderr 超时脱离、CLI 未知字段、Windows pre-lock 元数据探测和 Unix reap 后 PGID 复用已修复并重验；普通命令已观测的退出/输出保留，清理 Unknown 不被洗掉。
- 首轮完整旧 smoke 在 paused-plan 的 terminate→resume 流程失败；明确区分 crash 持久性与 graceful 全局取消，原布局/修复验收断言不变，另加真正 SIGTERM 取消 paused 并拒绝 resume 的用例。
- 第二轮 package 的 freshness 将 daemon binary main 错当作 CLI 链接依赖，因而在实际复制/执行前拒绝。仅排除该非依赖，daemon 自身 main、CLI 的 daemon library、manifest/lock 和源码摘要仍保留，新增 synthetic 回归；未触碰二进制时间冒充重建。
- 私有候选的 node_modules 链接触发 pnpm 依赖引导并因环境路径不可用退出；没有安装或修改环境。随后逐 SHA 核对 7 个真实 Web 输入与候选完全相同，在已有主工作区依赖上重跑 4 组通过，记录 `drain-web-source-proof.json`。不把首轮环境失败记为通过。
- 初期 fixture 拼写/缺字段及 Clippy 风格失败日志保留；最终原本要求的拒绝/唯一请求/不重放断言未放松。

## 已发布提交与精确 CI（2026-10-29 06:34 UTC）

本节采用主任务核验的精确提交/run 终态；本次文档刷新不运行测试。

- 网关撤权夹具修复 `c73a62afe446ed089ff6a80087b447c5eb7610de` 的 [push 40821487804](https://github.com/YS1109/self-agent/actions/runs/40821487804) / [PR 40821493624](https://github.com/YS1109/self-agent/actions/runs/40821493624) 均 **6/6 成功**，不借用为后续 v15/v16 的结果。
- v15 `7aea4f15f04a4ce3ffdbf944fdc454160dcc465c` 两轮 CI（push `40827179414` / PR `40827184463`）均终态 **5/6**，仅 Windows Rust 被 `storage.rs` 的 `unused_mut` 阻断，Windows 后续测试未运行；后续生产等价 cfg 窄修复 `43d12ee3bc7f708cbe55101b7a12ce63480f0f68` 的两轮 CI（push `40828345766` / PR `40828352198`）也均终态 **5/6**：macOS / Ubuntu Rust 全部 smoke 与三平台 Desktop 成功；Windows 产品 Rust / update 4 组已通过，仅最终 package smoke 中文输出遇到 Python cp1252 编码失败。两次失败均保留；详情见 [更新验收](acceptance-update-v1.0.md)。
- v16 `925e6cf2d924141a0cff972113d5b37556891de9` 的 [push 40828936545](https://github.com/YS1109/self-agent/actions/runs/40828936545) / [PR 40828942266](https://github.com/YS1109/self-agent/actions/runs/40828942266) 均终态 **4/6**：三平台 Desktop 与 Ubuntu Rust 成功；macOS daemon **790/791**，僵尸进程组仍可被信号探测的夹具失败；Windows daemon **728/730**，两个新 CMD / Job 复合脚本夹具失败。平台测试已实际运行，不能再称 Windows 仅交叉编译，但也不能声称这些平台的完整生命周期链路已经通过。
- 进程夹具及 package 输出编码的窄修复已完成，**本地重新验证见下节；新精确 CI 尚未取得**。本地旧冻结 core13 / daemon791 / CLI53 / gateway17 / UI343、22 个 smoke、Web 4 组仍只证明原固定候选，不自动证明这些修复或远端全绿。
- 此前 GitHub 内容上传曾因额外披露确认暂停；用户随后已明确授权 v15/v16 与 QQ 相关代码的开发分支发布，阻塞已解除。该历史拒绝不再是当前待办，也未扩大为真实 QQ 通讯、部署或正式发行授权。

## 未运行 / 仍待处理

- 发布已独立复核及重新验证的窄修复，跟踪其精确 push / PR CI；当前失败之后未运行的后续阶段不能算通过。
- 实机 GUI、真实 OS vault、真实模型/QQ、公网 nginx、原生安装/升级/卸载、生产签名/发行均未运行。用户授权的是模拟验证和开发分支，不是部署或正式发行。

## 本次跨平台 CI 修复的重新冻结（2026-10-29 06:46 UTC）

本次以 `925e6cf2` 为父提交，修改仅限以下两类生产/夹具边界和验收文档，没有放松权限或物理停止证明：

- Windows `cmd.exe` 不使用 CRT argv 解码。仅明确的 `/d /v:off /s /c` 加一个完整脚本采用 CMD 原样脚本编码，拒绝歧义参数、NUL 和超长 UTF-16；其它 EXE、batch shim、Native PowerShell 路径、Job 与 no-follow 锁均保留。新增引号路径/重定向/PowerShell 参数测试，原退出码和 root 退出后仍存活后代断言不变。
- macOS 不把 zombie 的进程组信号可见性当作可移植断言；再次真实 `waitid(WNOWAIT)` 证明 exit 7 且尚未 reap，保留最终 PID 清除、exit 7 和进程组消失检查。生产 EPERM→Unknown 规则不变。
- package 夹具显式使用严格 UTF-8 控制台输出；不修改动作、超时、数据保留或 freshness 断言。详情见更新验收。

已重新运行并终态通过：core **13** / daemon **791** / CLI **53** / gateway **17**，fmt、四 crate all-target Clippy、daemon/CLI/gateway 与 update fixture 构建；全部 **22 个后台 smoke**（含真实关闭 6 组）以及 history benchmark self-test、package **6 项** self-test。另在 cp1252 环境下单独运行同一 6 项 package self-test，通过。源码与 4 个二进制前后 SHA-256 一致，证据为 `drain-ci-fix-gate-receipt.json` 和 `/tmp/drain-ci-fix-{fmt,rust,clippy,build,smokes,package-self-tests,history-self-tests}.log`。

独立源码复核通过；Linux Native 52 / Codex 47 定向通过。Windows **13 项仅交叉编译及 Clippy**，不是新 Windows 执行结果；修复后的真实 Windows/macOS 运行仍须新精确提交 CI，不能沿用失败父提交或本地 Linux 结果。前端未修改，此次未重跑 GUI/前端；前述 343 与原三平台 Desktop CI 是其各自的历史证据。

## Windows 旧运行时路径跟进（2026-10-29 07:07 UTC）

`df0657906a7b41f77c49ebac5ef9e0ee48da655f` 的 [push 40831626789](https://github.com/YS1109/self-agent/actions/runs/40831626789) / [PR 40831632978](https://github.com/YS1109/self-agent/actions/runs/40831632978) 两轮终态均 **5/6**：macOS / Ubuntu Rust（含全部后台 smoke）和三平台 Desktop 通过；Windows daemon **732/733**，原 CMD/Job 与新增 CMD 引号路径全部通过，但新增 PowerShell 测试在一轮报告 ServicePointManager 初始化异常/exit -65536，另一轮达到原 10 秒观察截止。Windows 后续 smoke 未运行；不把新失败写成平台已修好。

本次仅改 Windows launcher 与其测试：不再把 `GetFinalPathNameByHandleW` 返回的设备路径拼写直接传给子运行时，而由已持有的同一 no-follow handle 得到 canonical 路径，再严格验证普通 DOS 等价表示。保留所有祖先/程序锁、Job、环境、权限和清理确认，歧义/长路径明确拒绝；支持上限见协议 v16。微软 `GetModuleFileNameW` 和 .NET ReferenceSource 配置路径传播语义支持此兼容性假设，但泛化的初始化异常不足以单独证明内层根因。

回归在同一 Job/环境/脚本条件下先运行普通 DOS，再观察旧设备拼写；每次仍独立限 10 秒并等待原 Job 清理，只输出退出码/超时/固定异常分类。正常路径仍严格要求 exit 7、原引号/运算符 stdout、指定 stderr 和 Job 空；旧路径失败不是通过条件，也没有生产自动重试/回退开关。

独立源码复核通过；Linux Native **52/52**、Codex **47/47**、daemon all-target Clippy、fmt / diff check 通过；Windows **14 项交叉编译及 Clippy** 通过。本次改动完全在 Windows cfg 模块，未重新运行完整 Linux/前端或复用旧二进制假称新 smoke。真实 Windows A/B 和完整门禁须在本次精确提交 CI 验证，当前仍未确认有效性。

证据：`/tmp/drain-powershell-path-{native,codex,clippy}.log`、`drain-ci-fix-ci-final.json`；Windows 源码 SHA-256 `e2463a90b83f952062773f1c88b0dcfe0fdc63a7a3c12062324c0883c92ed6b4`。
