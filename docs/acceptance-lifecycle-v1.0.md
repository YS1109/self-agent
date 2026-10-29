# 1.0 实际后台生命周期验收记录

状态：本地源码已冻结、独立审查和最终模拟门禁通过；尚未发布，精确提交 CI 未运行。此前隔离 helper 测试不能替代以下实际 daemon 退出证据。

## 已取得的定向证据

- lifecycle runtime/control/Store 当前过滤 **76/76** 通过，包含实例绑定、真实 worker registry、两次 quiesce、observer 断连、tracked settings 读取、启动失败真实清理与 SQL 已提交后 Err 不误称回滚。
- 真实 owner HTTP / worker 定向：命名 Provider **7/7**、MCP 凭据 **6/6**、vault **18/18**、MCP stderr 真实析构等待 **1/1**、backup API **10/10**、history API **4/4**。覆盖 vault backend Drop 阻塞不得误报停止、关闭后新意图拒绝、配置/历史读取 observer 断连仍等待原 worker。
- CLI 生命周期 **3/3**：精确响应字段/一致性、显式确认、实际 TCP 关闭响应丢失仅一次 POST。后续实际 daemon+CLI 成功流程另由 smoke 验证，不以 TCP 夹具代替。
- QQ **126**、Gateway **63**、Update runtime **17** / Store **5** / API **5** 定向通过；原有发送结果与当前物理清理分开。
- Native bridge 实际源提取 **27** + 专用子进程入口 **1** 通过；已观察 peer 离线不会自动启动替代后台，关闭观察的 HTTP 等待限 35 秒。不是完整 Tauri/GUI。
- UI controller/component/proxy 定向 **27**、完整前端 **343/343**、TypeScript 与 Web/mobile 构建已通过；不是已发布提交或完整 GUI 结论；最终同一源码的固定二进制回归见下表。

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

私有冻结候选 `self-agent-drain-final/`、398 文件和二进制清单 `drain-frozen-candidate.json` 位于执行工作区，未写入 GitHub。最终二进制 SHA-256：

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

## 未运行 / 仍待处理

- 精确提交与三平台 CI。当前工作树包含未发布 v15 和本片；Windows/macOS 实际生命周期、Windows Job 运行结果尚未取得，不能以 Linux 或交叉编译替代。
- 实机 GUI、真实 OS vault、真实模型/QQ、公网 nginx、原生安装/升级/卸载、生产签名/发行均未运行。用户仅授权模拟验证和开发分支，不授权部署或发行。
- GitHub 窄 fixture 上传因明确的 auto-review 披露拒绝而暂停，额外用户确认仍待回复；没有换工具、代理或 tree content 绕过。当前没有新发布 SHA，以上本地证据不等于远端通过。
