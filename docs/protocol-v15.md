# 协议 v15 · 本机发布校验、私有暂存与兼容握手（开发切片）

本合同已接入本机 Store/DataLease、schema v4、owner HTTP、CLI 和桌面只读状态，当前开发切片的最终门禁与发布记录见验收文档。生产自动下载、安装、执行与生产签名身份保持禁用/未配置；本轮只使用临时进程内签名 key 和本地夹具，不登记用户实际信任根。

## 两种权限必须分开

- 生产自动更新 policy 固定禁用；环境变量、清单、上传请求都不能开启它。没有网络下载、解压、子进程执行、安装或启动 staged artifact 的路径。
- 独立 **verification-only** 配置必须由已认证本机 owner 预先确认：固定 HTTPS source（允许固定路径，不含 userinfo/query/fragment）、Ed25519 公钥指纹、key ID、明确修订和启停。它只决定是否允许校验/暂存，不授予生产发布或安装权。
- 单次校验请求只引用既有 trust revision，不能携带替代公钥/source。更改配置使用 CAS，旧在途操作和旧暂存的当前授权失效；不抹除历史验证事实或降低高水位。
- QQ/H5/模型/MCP 无权调用这些管理入口，桌面桥也不能把任意 URL/路径变成更新执行目标。

## 签名与产物

签名覆盖 `self-agent-update-manifest-v1\0` 加 **原始 UTF-8 JSON 字节**，不是重新序列化后的内容。使用 Ed25519 严格校验；清单 16 KiB，上限产物 256 MiB。绑定 application=`self-agent`、stable channel、固定 source/key_id、当前平台、数据库 schema 范围、稳定 SemVer、单调序号、有效期、真实长度与 SHA-256。未知字段、重复字段、预发布/build metadata、路径式产物名、越界/截断/多余字节均拒绝。

签名中的产物 basename 永不用于本地路径。真正读取/散列/私有写入/同句柄复读校验后，暂存使用固定 operation UUID 派生的私有文件名，create_new / nofollow / no-replace，Unix 0600。没有自动执行，即使签名/哈希正确也只是一份待用户审查的字节。 文件 open/write/flush/fsync/seek/readback/link/remove 在被持有的 staging blocking worker 内同步完成，只有输入读取可取消；不会通过丢弃 Tokio 文件 I/O future 提前释放实际写入所有权。内核调用不可抢占，文件移除 best-effort，残留 orphan 永不自动提升为权威。

有效期在接收清单、上传前、追加 witness 前及最终成功事务内检查；上传期间过期不能仅凭起初有效而成功。单条暂存复核重新验证签名/有效期/当前程序与 schema，再散列实际文件，散列后再次导入 witness 并核对相同配置/receipt/floor；返回值是该读取的观察，不是后续执行许可。

## 原始回执与中断

提供 owner 元数据 intent 与单次流式 artifact upload；artifact 不嵌进 JSON/日志/快照。原 operation ID、trust revision、exact manifest/signature 摘要与已验证 metadata 固定。同 ID 只读原结果，内容冲突拒绝；已领取的上传/OS 工作不得再次领取。HTTP 断开不使已接收工作失去管理，但输入流 EOF/错误会失败关闭。

SQLite 将配置/意图/领取 nonce/结果与 floor 作为事实源。重启不自动重放 pending/verifying；留下 interrupted/unknown 事实，不把 orphan 文件提升成可用 stage。成功暂存仍不是安装。使用前重新校验当前授权与字节；本切片不提供 installer 消费接口。

最多 256 项验证历史、单个流式上传/暂存工作、总 staging 空间 512 MiB / 512 个私有名称；写入前预留临时 .part 与发布硬链接两个名称槽。上传截止 120 秒，元数据工作最多 8 个；这不宣称内核文件系统调用可以抢占式终止。超过容量失败关闭，不能通过删除未知事实或 floor 自动腾出名额。清理只针对明确 operation UUID 的私有 stage，必须匹配其元数据，永不删除 floor、任意路径或真实用户安装文件。创建失败不能删除本次未拥有的既有 .part；见证拒绝链接/特殊文件，Unix 非阻塞打开避免 FIFO 卡住验证工作。

## Owner API / CLI / 状态界面合同

所有入口必须位于现有本机 bearer 认证之内，不加入 QQ/H5 RPC 白名单。JSON 元数据最大 128 KiB（其中原始清单最多 16 KiB）；不回显底层输入/路径错误。artifact 是有界原始字节流，要求唯一规范 Content-Length 与 `application/octet-stream`，在领取前与原意图长度一致，随后仍检查真实 EOF/摘要。没有 URL fetch 或任意路径下载参数。

| 路由 | 行为 |
| --- | --- |
| `GET /api/v1/reliability/updates` | 固定 mode、配置、6个target floor、最近最多32条历史、配额/阻断状态；历史不声称当前 stage authority |
| `POST .../trust` | expected_revision、公钥hex、key_id、固定source、enabled、独立confirmation_fingerprint；CAS未知只读刷新，不自动重发 |
| `POST .../verifications` | id、config_revision、raw_manifest、signature_hex；创建固定元数据意图，不上传产物 |
| `PUT .../verifications/{id}/artifact` | 原ID仅一次领取流式验证；旧/终态ID不重传 |
| `GET .../verifications/{id}` | 只读原receipt，重新确认current_stage_verified；始终installed=false、automatic_update_enabled=false |
| `POST .../verifications/{id}/cancel` | 持久取消；不把请求被接受当作清理已经完成 |
| `POST .../stages/{id}/discard` | expected_sha256限定原私有暂存；保留floor/receipt，不触任意路径 |

CLI `updates status|trust FILE|-|verify REV MANIFEST SIGNATURE ARTIFACT|operation ID|cancel ID|discard ID SHA256`。先协商已认证 daemon；verify 显示原 UUID，只提交一次 intent/一次 upload。响应缺失或ID/metadata不符不继续上传、不创建新ID恢复。UI只提供只读状态和原UUID复核，明确配置、历史成功、当前暂存与安装四者不同；实际文件上传/信任配置由本机CLI完成，没有伪“安装”按钮。

## schema v4 与私有目录

v4 ledger 明确增加 update_trust/update_jobs/update_floors；升级先验证旧 v3 形状及既有 ledger，再在单一事务内迁移并验证新结构，不接受未登记 update 表。更新运行时复用同一个 Store mutex/SQLite 事务，而非另开事实库。DataLease 保留至真正 detached 上传/散列/见证/最终事务完成；HTTP 观察者消失不能释放数据目录独占锁。stage/witness 从 DataLease 的已 pin 父目录以 nofollow 打开，不能按环境/HTTP 参数重开任意目录。

## 回退保护与备份恢复

floor 按 self-agent/stable/target 作用域保留最高版本和序号，不随 key/source 修订重置。只有版本与序号均严格提高才可接受新产物；原 ID 的只读回执不等于新验证。相等 pair 的不同 manifest/artifact 摘要为不一致，交叉高低拒绝。

为避免恢复旧 task DB 降低 floor，数据目录内独立私有 witness 子目录不随 task SQLite 备份回滚。每次实际字节验证后，先追加并同步固定 UUID witness，再提交 SQLite 成功回执；生效下限为当前程序版本、DB floor、有效 witness 的保守并集。最多 256 份 witness，历史不可自动删；损坏、部分、链接或不一致记录阻断更新验证，不影响普通会话读取，也不回退到旧 DB 下限。

崩溃可能消耗一个版本/序号而没有成功回执，这是保守失败，不代表已安装。恢复任务库禁用 verification trust、撤销旧 stage 当前权威，保留历史 receipt，重新同步独立 witness 后才允许本机 owner 重新确认配置。恶意同用户删除整个私有数据目录不在本地文件信任模型内；不宣称硬件 monotonic counter 或通用 power-loss 安装事务。

## 协议兼容

采用认证 `GET /api/v1/compatibility`，严格有界 512 字节 Handshake，wire generation 1，与 docs 协议编号和 package semver 分离。core 唯一类型与 `negotiate` 复用现 lifecycle helper 的范围语义，并要求 `capabilities:{bounded_history:1}`，使客户端不会把窗口 snapshot 误当全历史；缺失/畸形/无交集失败，不把 missing 当兼容。握手只做兼容性，不替代 bearer 认证。

CLI/桌面桥/dev-proxy 必须先绑定已认证 loopback daemon 再协商，失败时不执行后续动作、重放命令、隐式停止或换一个后台。已知客户端请求带明确 generation header；服务端拒绝显式错误 generation。既有 owner HTTP v1 未提供 header 的兼容行为单列，不能据此宣称这些旧调用者进行过协商。实际 draining/stopped 状态由后续 lifecycle 接线提供，不能由静态 helper 冒充。

## 本机启动就绪与超时

桌面桥将“子进程创建”“已认证协议就绪”和“应用功能可用”分开。前置 metadata 读取和已认证兼容探测分别最多观察 2 秒；metadata 要求 nofollow 普通文件、最多16KiB，Unix 非阻塞打开，拒绝 FIFO/链接。该观察上限不宣称内核 I/O 可抢占终止；读取不确定不能触发替代进程。事件重连复用同一读取。只创建一次受信任安装位置的后台进程，保存退出观察与原始启动时刻；在 **30 秒原始绝对截止**内持续只读探测并显示中文加载进度。响应但缺少/不兼容握手的进程不是离线，不能自动停止、替换或把动作发给它。进程退出、暂未确认和等待超时分别保留可操作错误；所有早期错误同样进入带修订的内存状态，界面先订阅再读取该观察并拒绝旧响应覆盖，避免挂载前事件丢失；超时不证明进程已经停止。

超过截止后刷新只能做有界只读就绪探测；原进程稍后就绪可继续使用，但不能重置等待预算或再次创建进程。启动检查/读取失败不重发业务副作用。事件重连重新读取本机元数据并协商；仅暂时的连接/认证轮换可做只读重试，明确协议不兼容停止流。

此策略针对已有 5,000 任务真实容量测量中 7.153/6.836/5.702 秒启动超过旧 6 秒桥等待的风险，不改动原性能基准阈值。真实受信任测试子进程延迟 6.5 秒，以及较短测试截止后迟到就绪/单次启动的夹具验证了就绪观察路径；不把延长等待称作数据库启动优化、死锁修复或真实 GUI 验收。若 30 秒仍未就绪，界面明确提示检查日志、数据目录锁和磁盘，不能无根据自动重启。

## 包验证边界

包生命周期夹具只复制 **本次仓库构建的固定 daemon/CLI/gateway 二进制** 到临时安装布局，验证启动、兼容调用、重启/历史保留和移除临时二进制不删除数据。不运行下载/上传的 artifact，不把该夹具冒充 Windows/macOS/Linux 原生安装器、签名证书、真实升级/卸载或 GUI 验收。生产发布身份/源和真实安装仍需用户决定与独立验证。

官方实现参考：[ring Ed25519 verification](https://docs.rs/ring/0.17.14/ring/signature/struct.UnparsedPublicKey.html)、[semver precedence](https://docs.rs/semver/1.0.28/semver/struct.Version.html#method.cmp_precedence)。
