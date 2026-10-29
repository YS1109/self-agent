# 协议 v16：同一后台实例的实际停止与清理

本协议是 1.0 生命周期切片，不是发行版本号。桌面、HTTP 和 CLI 仍共享同一 Rust 后台。它不授权部署、发布、真实模型调用或系统凭据联调。此前 v15 的签名验证/私有暂存仍不是自动安装器。

## 1. 本机控制与实例绑定

仅经本机 owner bearer 及协议中间件提供：

- `GET /api/v1/lifecycle`：安全的进程内生命周期快照。
- `POST /api/v1/lifecycle/shutdown[?wait_ms=N]`：严格 JSON `{ "expected_instance": "当前快照的 UUID" }`。请求体最多 128 bytes；只接受这一字段。目标 UUID 不等于当前实例时返回 409，不能关闭新实例。
- `wait_ms` 省略/0 时立即返回 202；1–30000 时观察同一个已登记的停止监督任务。确认停止返回 200，其余返回 202。重复、未知、非整数、负数、越界参数拒绝。断开观察者不会取消停止，也不会重新发起执行。

快照包含 `instance_id`、`state`、`phase`、`outcome`、`workers`。随机 `instance_id` 在当前后台进程生命周期中固定，不是凭据或跨重启恢复令牌。`state` 是 running/draining/stopped；phase 依次为 not_requested、initial_quiesce、workers、final_quiesce、finished；outcome 为 null/stopped/unknown。worker 投影只有静态状态和计数，不包含任务内容、路径、密钥、原始错误或进程命令。

**连接失败不等于停止。** 旧实例的请求结果不能由新实例状态代替；观察截止仅结束等待，不杀死原 worker。未知物理清理保持 draining/unknown 和数据目录租约，不自动重放、重启或宣称成功。

## 2. 接受新工作与原工作结算

每个真正的异步/阻塞/进程工作必须先在同一 `LifecycleRuntime` 中保留容量，再开始持久意图。新事务的 gate 复核与实际 Store 操作持有同一 Store mutex；不能用 HTTP middleware 一次判断替代事务边界。锁序为 Store → intent gate → worker registry；关闭入口先关 gate，释放 runtime 锁后再取得 Store。

- 同步纯 SQL 校验/事务返回后明确结算 admission，表示这段同步执行已结束，不证明事务一定回滚。提交结果不确定仍保留原意图/审计，不自动重放；普通错误请求不会凭空变成物理清理 Unknown。该闭包不得启动 OS 或异步副作用，否则必须另行登记真实所有者。
- 已接受的独立异步操作立即将真实 JoinHandle 转入 registry；HTTP 请求只持观察 receiver。已武装但未结算/转交的 permit 异常丢失记为 Unknown。
- 原已登记 worker 的结果、credential receipt、backup manifest、update witness/floor 与 outbox 原结果仍可结算。不能把新请求伪装成 settlement，绕过关闭 gate。
- 新模型/工具/进程/文件读取、任务领取、远端输入及新的管理写入在停止后拒绝。纯控制状态和已有安全 receipt 可继续只读观察。

registry 的当前在途上限为 256，包含固定调度器与清理所有者；容量不足不接受新工作。它不是持久历史保留上限。

## 3. 停止顺序及真实所有权

HTTP 控制、Ctrl-C、Unix SIGTERM、已登记 worker 的 panic/Unknown 均汇合同一个 retained supervisor。Unix 信号流在 Vault/执行 worker 创建前注册并保留，重复信号不引入另一条强制成功退出路径。主进程启动后持续驱动 registry monitor，即使没有 HTTP 轮询也会发现失败。

1. 同步关闭 admission，发本地 StopSignal，停止 QQ/Gateway 输入和新派发。
2. 同 Store 锁执行首次 quiesce：先取消运行、暂停、等待输入或回顾中的计划，再取消排队/运行/等待/暂停任务及编排，阻止迟到完成推进后继；未发送的远端输入/通知/回调不新发。原 sending/unknown、nonce、已决定授权保留事实。
3. 等待实际 worker：任务与命令进程清理、阻塞文件/SQLite、凭据 OS 线程、备份导出、MCP probe、QQ/Gateway 原发送、更新上传/校验/witness/finalize 都由原所有者等待。取消观察 future 不释放这些工作槽。
4. 真实工作结束后第二次 quiesce，处理例如已知 429 返回 pending 的迟到投影；只有当前所有权和最终检查均确定才发布 stopped。
5. 主进程仅在 stopped 后结束 HTTP 服务并等待已接受响应结束，再释放运行时和数据目录租约。未确认清理不走成功退出分支。

控制路由的等待者和监督任务本身不进入被其等待的 worker 集合，避免自等待。正常关闭不是备份恢复：不主动撤销 QQ/Gateway 配对或配置；恢复仍执行其独立撤权规则。SIGKILL、进程崩溃或外部超时强杀无法提供优雅停止证明，重启依旧保留 interrupted/unknown 并禁止自动重放；故硬崩溃夹具明确使用 kill，不把 SIGTERM 当作崩溃。

## 4. 物理清理与历史未知分开

历史工具调用、credential mutation、QQ/Gateway 发送可以保留 outcome_unknown，即使原进程/连接已确定关闭；这些历史事实不应使以后每个后台永远不能停止。反过来，SQL 中任务 terminal 或 receipt completed 也不能证明当前外部工作已经停止。

OS vault 保留真实标准线程 JoinHandle；逻辑 STOPPED 标记不足以确认线程退出，必须在 backend 析构后实际 join。调用超时/断连可能得到未知结果，但全局 custodian 仍持有 OS 线程和租约，直到真实停止。MCP close 的 Unknown、失败结果落库或真实 worker 所有权缺口按当前不确定性传播，不能吞掉。

Native 阻塞读在取消时仍等待原线程结束。进程清理由专用 backend 报告，不能忽略 kill/wait 错误。Unix 的保证针对所启动的普通进程组，不是恶意 setsid 脱离的安全沙箱；Windows 普通树通过独立 Job launcher 接线，实际平台执行结果必须由精确 CI 记录，不能用 Linux 编译冒充。

## 5. 客户端行为

CLI `daemon-status` 只读；`self-agent-cli --wait 30 daemon-stop --confirm` 先读取并固定实例 UUID，再只发送一次带该 UUID 的关闭请求，`--wait` 为 1–30 秒。未知响应返回非零，不能自动再 POST；不得把连接拒绝或不同实例的快照打印为原实例已停止。

桌面可靠性面板明确展示关闭影响和一次确认，说明运行、排队及暂停任务/计划会取消、重启不自动恢复；正常关闭不是保留 paused 供 resume 的操作。只将固定观察 UUID 保存在本机 sessionStorage；页面关闭/后退/重载恢复时只读，不保存路径或密钥、不重发。新实例不能隐式替换目标；结束本机观察不改变原实例结果，操作新实例前必须重新读取并确认。

Native bridge 一旦观察过已验证的后台实例，就不因其离线自动启动替代进程；仍可只读发现所有者显式启动的新实例。首次正常启动的 single-attempt/readiness 规则沿用 v15。关闭桌面窗口不等于关闭共享后台。

## 6. 验证范围

实现、fixture/loopback、真实本机子进程、跨平台精确 CI、实机 GUI/OS vault/外部模型是不同层次。最终验收记录在 `docs/acceptance-lifecycle-v1.0.md` 追加；开发中的定向通过不等于完整 1.0 验收。无生产签名、下载执行、安装、nginx 部署或真实 QQ 消息声明。
