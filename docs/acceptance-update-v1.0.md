# 1.0 本机签名校验、暂存与兼容切片验收

当前状态（2026-10-29 07:28 UTC）：当前已验证代码基线为 `8a5b980a9675d64ed74e192c8fad28fe94cf0ca1`（tree `8c99cc7475d099d3ed497b5457d1364f5898da91`）。[push 40833625420](https://github.com/YS1109/self-agent/actions/runs/40833625420) / [PR 40833632597](https://github.com/YS1109/self-agent/actions/runs/40833632597) 均终态 **6/6 成功**，分别于 2026-10-29 07:27:46 / 07:26:37 UTC 完成，三平台 Rust 与三平台 Desktop 共 12 个作业均成功。此结论仅属于该代码提交，不是尚未提交的本次文档刷新之 CI。

本记录只覆盖协议 v15 / schema v4 的开发实现，不把它称作生产更新器、安装发布或整个 1.0 完成。所有签名材料均来自临时进程内 Ed25519 key；没有登记用户信任根、生产密钥或访问真实更新源。

## 已交付边界

- 真实同一 Store / DataLease / schema v4：独立 owner verification-only trust、CAS 修订、固定 source/公钥指纹、原 UUID 意图与不可再次领取的流式上传、私有 nofollow 暂存、SQLite floor 与外部 append-only witness。旧库恢复禁用信任/暂存权威，不降低仍存在的 witness 下限。
- 真实 owner HTTP 与 `self-agent-cli updates` 可配置验证信任、提交原始签名清单/有界字节、查询/取消/明确清理原 stage；未知不自动重发。UI 仅只读状态和原 UUID 复核，不伪造安装或上传按钮。
- 已认证 wire-generation 握手覆盖 CLI、Native bridge/事件重连及开发代理；缺失/畸形/不兼容 peer 不接收后续动作，不被隐式停止或替换。
- Native 启动区分一次进程创建与协议就绪：前置 metadata/握手各自有界；已登记启动原始等待上限 30 秒、保留退出观察、明确进度/错误，之后只读发现迟到就绪，不重新启动。内存修订状态保证早于 UI 挂载的错误仍可读取，旧 snapshot/event 不覆盖新状态。

## 本地门禁（2026-10-29）

| 检查 | 结果与证据边界 |
| --- | --- |
| 精确候选完整 Rust core / daemon / CLI / gateway | 13 / 683 / 50 / 17 项通过；I/O窄修后重新完整构建与验证，不混入后续drain源码 |
| fmt / all-target Clippy / daemon+CLI+gateway+fixture 构建 | 通过；本地 Rust 1.96，精确 CI 使用其已配置工具链另验 |
| 前端 / TypeScript / Web+H5 构建 | 331 项通过，双构建通过 |
| Native 纯 Rust 源提取 | 25 项通过，另1项专用 ignored 子进程入口由真实进程测试显式调用；不是完整 Tauri 构建或 GUI |
| 真实启动观察 | 受信任测试子进程延迟6.5秒后就绪；较短测试截止后迟到就绪不二次启动；FIFO/链接/目录/超长 metadata 拒绝；早期错误/新旧修订/卸载订阅回归通过 |
| 实际 Store/DataLease | 4项：v3→v4保留ledger与历史、detached上传继续持锁、已pin目录不被换链接重定向、真实旧备份恢复保留独立更高floor |
| 实际 owner router | 无bearer拒绝；认证握手有界/no-store；错误generation在解析/写trust之前拒绝 |
| 更新联合夹具 `update_control_smoke.py` | I/O窄修后固定新二进制重跑4组：独立owner信任、真实CLI单次字节验证与历史不重传、篡改失败不消费下个release、重启与旧库恢复保留witness下限 |
| 包布局 `package_lifecycle_smoke.py` | I/O窄修后固定新二进制重跑4组；原4项self-test通过：只复制当前固定构建、旧/损坏握手零动作、daemon/CLI/gateway进程重启保数据、仅移除复制二进制 |
| 既有后台回归 | 19个smoke全部通过；history benchmark self-test通过，不冒充重跑5,000任务性能基准 |
| 实际开发代理 | 4组：中文SSE、断开清理、请求/Origin边界、不兼容peer不转发、游标冲突与私有token轮换 |
| 暂存 I/O 所有权窄修 | primitive+witness 24项；候选真实 update/state/witness/runtime/API 53项与 all-target Clippy通过；同步写入后暂停夹具证明取消不提前释放owner/清理，不宣称可抢占内核写入 |
| 独立只读源码审查 | isolated verifier及实际Store/API/客户端/启动接线均clear；问题修复后复读，审查者未代跑构建或真实调用 |

本地日志索引（执行工作区 `/tmp`）：`update-full-rust.log`、`update-integration-cli.log`、`update-full-clippy.log`、`update-ui-full.log`、`update-native-integration.log`、`update-store-integration.log`、`update-control-smoke.log`、`update-package.log`、`update-all-smokes.log`、`update-web-smoke.log`、`update-primitive-io-fix.log`、`update-v15-recheck.log`、`v15-exact-final-gates.log`、`v15-exact-update-smoke.log`、`v15-exact-package-smoke.log`、`v15-exact-readiness.log`。这些日志不随仓库发布、不含生产凭据。

## 审查闭环

创建失败不能删除未拥有的.part；witness FIFO非阻塞拒绝；签名有效期在实际成功边界复验；写前预留两个名称槽；散列后重查撤权/floor；完整 raw manifest digest 必须匹配 CLI 回执，错digest真实HTTP夹具证明零PUT。UI刷新即清旧stage观察；启动初查及事件重连共享有界metadata读取、Windows显式拒绝reparse；早期错误进入可重读的有修订状态。

文件写入、flush/fsync、seek/同句柄复读、link/remove 留在实际被持有的 staging blocking worker 中；只有输入读取可取消，避免 drop Tokio 文件 I/O future 后留下未被持有的内部工作。私有文件移除仍是 best-effort；未清理 orphan 不获得暂存/安装权威，v15 原片当时未接统一物理 drain；后续 v16 已接线，独立证据见 [生命周期验收](acceptance-lifecycle-v1.0.md)。该两文件修复已独立复读 clear。

## 同一 5,000 任务数据的实际就绪补验

仅复制已保存的合成 seed，不修改原始数据，不重跑整套分页基准。固定新 daemon 的 SHA-256 为 `27ca4cf6b5442d0c231cca9c679578bd60c9c2d26542ebaa3243c3cb73718a8d`；测量输入源码树为 `105a702e4c9a5a42b756bf22f77e3943704f3ce7`，此后只更新验收文字。固定CLI/gateway/example摘要另保存在工作区 `update-exact-binary-sha256.txt`。

从创建进程到成功完成已认证、最多512字节的兼容握手，共用原始 **30秒** 截止，不在health或握手时重置：

| 样本 | compatible readiness |
| --- | --- |
| schema 1→4 首次迁移（n=1） | 11,598.7 ms |
| 温热重复启动1 | 20,335.0 ms |
| 温热重复启动2 | 3,523.3 ms |
| 温热重复启动3 | 3,165.8 ms |

4/4通过，但保留20.335秒慢样本；这是共享Linux开发构建环境的headless实际进程观察，不是冷缓存、桌面GUI、latency SLA或数据库启动优化证明。它与真实6.5秒子进程的Native启动观察测试共同覆盖旧6秒截止不足的已知场景，不保证任意历史/磁盘负载都在30秒内完成，超时仍必须诚实显示未确认且不替换后台。

原seed SHA-256 `0970b82d6d804fb4472e56f27d9a1199f2e4a881b083f320e99d37085366c4cb` 前后相同。副本迁移及每轮启动后，100个session、5,000个completed task、10,000条message、5,000个command、20,000条保留event完全不变；全部历史内容摘要仍为 `ec2f9d3d711239e64640424e1c2ba152c494a32fb499f067fd1604e1a8ce9ea0`。schema升级后数据库从59,211,776到59,662,336字节；已核对的完整历史没有裁短。

私有原始结果和测量脚本：`/workspace/scratch/ed5e723b0697/v15-large-readiness/`；结果 `measured-20261029/result.json` SHA-256为 `e3f357e921f9d6d6f4c08a7df36c611339010611498154ff78e61ac9b2327abd`。只启动固定构建与复制数据，不调用模型、平台、真实凭据，不执行上传产物；夹具进程终止不冒充后续优雅drain。

## 未运行 / 不交付

- 当前容器完整 `cargo check -p self-agent-desktop` 因缺少 `gobject-2.0`/WebKit系统库受阻；纯源提取测试不能替代完整 Native 三平台 CI。未进行真实原生 GUI 或手机视觉验收。
- 包夹具是临时布局/进程与数据保留检查，不是 OS 安装器、真实卸载、生产签名、自动更新或优雅 drain 证明。没有执行任何待验证产物、网络下载、生产 key/source enrollment 或部署。
- 30秒是明确就绪观察产品边界，不改变既有5,000任务测量，不宣称数据库启动优化/任意磁盘I/O可抢占或死锁已消除。
- v15 原片不含完整 lifecycle / drain；后续 v16 已把更新 worker 接入统一 registry，真实停止证据见生命周期验收。仍不能用 HTTP 观察者结束冒充 OS 工作 settled，也不把临时包布局当原生安装器。
- floor/witness依赖受保护数据目录；不防同用户恶意删除整个数据根，不宣称硬件单调计数器或通用断电安装事务。生产自动下载与安装始终禁用。

原冻结时尚待发布/CI；后续各精确提交结果见下文与顶部当前状态。本地门禁或其它 SHA 的 CI 不替代原提交结果。


## 首次精确 CI 与 Windows 条件编译窄修复（2026-10-29）

`7aea4f15f04a4ce3ffdbf944fdc454160dcc465c` 的 push `40827179414` / PR `40827184463` 两轮均终态 **5/6**，仅在 Windows Rust Clippy 报 `storage.rs` 的 `unused_mut`；该平台后续测试、构建及 smoke 尚未运行，不记作通过。三平台 Desktop、Ubuntu / macOS Rust 均成功。

修复仅将目录 builder 的可变绑定放入 Unix cfg 块：Unix 仍设置 0700，Windows 使用原默认 builder，创建、AlreadyExists、no-follow capability 和身份复核逻辑不变。独立静态复核通过；隔离源码的 storage 34 / Store updates 4、fmt、四 crate all-target Clippy 通过。精确生产 builder 片段的 Windows GNU compile-only Clippy 通过；完整 daemon Windows 交叉检查因本地缺少 `x86_64-w64-mingw32-gcc`（ring C 构建）未完成，不能冒充 Windows 执行通过。后续修复精确提交 CI 单独跟踪。

## Windows 控制台编码夹具修复

`43d12ee3bc7f708cbe55101b7a12ce63480f0f68` 的 push `40828345766` / PR `40828352198` 均终态 **5/6**，仅 Windows 失败；已在真实 Windows 越过原 Clippy 阻塞，Rust 测试、构建、既有后台 smoke、新 update 4 组与 package self-test 均通过；最后实际 package smoke 的首条中文 PASS 输出被 Python cp1252 stdout 拒绝，失败处理的中文消息同样不能编码。此结果不等于产品更新流程失败，但完整 CI 仍不记作通过。

后续修复仅在 package 夹具入口显式将 stdout/stderr 设为严格 UTF-8，不改变产品、动作、断言、超时或二进制来源检查。继承 v16 的 freshness 回归后共 6 项 self-test 通过：新增真实子进程 `PYTHONIOENCODING=cp1252` 下中文 help 与两路诊断输出解码验证。以完全相同修改后脚本源码、固定 v16 私有候选普通二进制运行的实际 package 4 组全部通过，完整保留 freshness/hash/原数据检查；未改写冻结候选源码或构建产物。此为 Linux 上模拟 cp1252 的本地回归，不能替代新精确 Windows CI。

证据：`/tmp/package-utf8-self-tests.log`、`/tmp/package-utf8-actual.log`。没有真实安装、生产签名/更新、GUI 或外部服务行为。

本次与生命周期平台修复联合冻结后，又用新构建二进制运行完整 **22 个后台 smoke**（包括 update 4 / package 4 / shutdown 6 组）、完整 Rust **13 / 791 / 53 / 17**、fmt / all-target Clippy / 构建与 package **6 项** self-test，均通过。日志见生命周期验收的新 CI 修复段；该冻结时真实 Windows 编码修复终态尚未取得；后续结果见下节，不以本地 cp1252 模拟替代。


## 最新提交的跨平台实际结果（2026-10-29 07:28 UTC）

`df0657906a7b41f77c49ebac5ef9e0ee48da655f` 的 push `40831626789` / PR `40831632978` 均终态 **5/6**：macOS / Ubuntu Rust 全部 smoke 和三平台 Desktop 成功；Windows **732/733**，新 PowerShell 初始化夹具失败，失败后的 smoke 未运行。原编码、CMD 与路径问题按精确提交分别保留，不相互覆盖。

当前已验证代码基线为 `8a5b980a9675d64ed74e192c8fad28fe94cf0ca1`（tree `8c99cc7475d099d3ed497b5457d1364f5898da91`）。[push 40833625420](https://github.com/YS1109/self-agent/actions/runs/40833625420) / [PR 40833632597](https://github.com/YS1109/self-agent/actions/runs/40833632597) 均终态 **6/6 成功**，分别于 2026-10-29 07:27:46 / 07:26:37 UTC 完成，三平台 Rust 与三平台 Desktop 共 12 个作业均成功。此结论仅属于该代码提交，不是尚未提交的本次文档刷新之 CI。

两轮 Windows 产品 daemon **734/734** 与全部 smoke 成功，包含 update **4 组**、package **4 组**和实际 shutdown **3 组**；macOS / Linux 信号关闭另为 **6 组**。真实旧 extended 路径 / DOS 等价路径 A/B 分别 exit -65536 / 7，`ServicePointManager` 异常分类分别 true / false，均未超时。固定 handle 身份、Job、权限和清理规则没有放松；Windows 普通本地路径的 UTF-8 与少于 260 个 UTF-16 code unit 限制见协议 v16，不是通用长路径支持。

证据为工作区 `powershell-path-ci-final.json` 与 `/tmp/powershell-path-windows-{push,pr}-proof.log`。这确认代码基线的模拟与跨平台门禁，不是运行待验证 artifact、生产更新源/信任根、签名身份、自动安装或原生安装器验收。包版本仍为 `0.6.0`，没有 merge、tag 或正式 release。
