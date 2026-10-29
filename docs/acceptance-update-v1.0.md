# 1.0 本机签名校验、暂存与兼容切片验收

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

文件写入、flush/fsync、seek/同句柄复读、link/remove 留在实际被持有的 staging blocking worker 中；只有输入读取可取消，避免 drop Tokio 文件 I/O future 后留下未被持有的内部工作。私有文件移除仍是 best-effort；未清理 orphan 不获得暂存/安装权威，本片不声称已接统一物理 drain 证明。该两文件修复已独立复读 clear。

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
- 新的完整生命周期/drain仍属后续独立切片；本片握手报告running，不伪造已接线draining/stopped。所有更新worker后续必须接真正统一registry，不可用HTTP观察者结束冒充OS工作settled。
- floor/witness依赖受保护数据目录；不防同用户恶意删除整个数据根，不宣称硬件单调计数器或通用断电安装事务。生产自动下载与安装始终禁用。

精确提交/CI：待此冻结树发布后跟踪，不能用本地门禁或其它 SHA 的 CI 替代。
