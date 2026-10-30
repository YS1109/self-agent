# 0.8 托管 MCP / Skills 切片验收记录

日期：2026-10-28。状态：本地冻结门禁已通过，精确提交 CI 待发布后记录。此记录不覆盖下一切片的命名模型服务或系统凭据完整生命周期。

## 实际验证路径

- 真实 Rust 后台、HTTP 管理入口、Native loopback 模型、真实 Python stdio MCP 子进程，均使用隔离临时目录。
- Skills 固定正文实际出现在模型请求中；公开快照只保留元数据，管理命令幂等且重复导入不重新读取。
- MCP 实际发现与工具调用分离，模型只看到任务固定目录的不透明别名。实际结果、调用状态和外部调用计数均检查，不以配置页状态代替调用。
- 已知密钥在文本、JSON 字符串键/值、转义和截断边界的脱敏回归。系统日志不记录 raw MCP 消息；host environment canary 不继承到子进程。
- 拒绝/预算耗尽时进程不启动；审批前无进程，审批包含具体脱敏参数预览与全部参数摘要；撤销或发现期间 Allow→Ask 收紧后不发送工具请求。
- 非字典序工具目录规范化后可以实际调用；不支持/畸形 schema 与不安全数字范围拒绝，不能静默略过约束。
- 发送后 EOF 为未知结果，重启保留未知且调用不重放。清理未知时隔离同项目后续任务；精确终态 owner 确认才解除，不能改写原任务结果。
- 同目录和祖先路径替换、stdin 背压、静默服务、半帧 UTF-8、通知/分页/流量限额、进程树清理、挂起进程失败路径由 Rust 夹具覆盖。

## 最终本地冻结门禁

- Rust：core **7**、daemon **272**、CLI **25** 项全部通过；格式、全目标 Clippy `-D warnings`、daemon/CLI 构建通过。
- 前端 **146** 项、Web 构建、Web 事件代理 **3** 组、同文件 Rust 事件桥无头检查 **7** 项通过。
- **15 个后台 smoke 脚本**全部通过；新增 `capabilities_smoke.py` 的 **7** 组覆盖实际使用、异步收紧与项目隔离。此前14个脚本重新执行，并非引用历史结果。
- MCP Linux 模块 **10** 组真实子进程测试通过。Windows 后端在 Linux 上完成官方 Windows target 的编译及全目标 Clippy，**10 组 Windows 测试已编译但尚未在 Windows 运行**，必须追踪发布后的对应 SHA CI。
- 曾失败的新增重启夹具补齐真实 session 后通过；UI/CLI 测试中的类型和 helper 编译问题已修复并纳入以上完整重跑。最终没有未解决的本地失败。
- Web 构建保留 Tauri 静态/动态混合导入和主 JS 约 515 KiB 分块建议警告，不影响退出成功；没有隐藏为无警告。

## 独立审查修复

目录规范顺序、JSON 转义前递归脱敏、审批参数可见性、安全整数边界、发现期间权限收紧、未知清理后的项目隔离，以及启动前取消的精确 NotStarted 分类，均以回归或有界夹具验证。调用意图与清理所有权已放入同一 SQLite 事务，取消不能产生无清理归属的 sending 记录。Windows 在同步打开前只接受固定磁盘，并锁定/检查根和程序各级路径、最终句柄路径；junction 回归已编译，实际 Windows 运行结论另记。

## 未运行 / 不在本切片范围

- 真实模型、真实 MCP 服务、真实凭据 enrollment/系统 vault 操作均未运行。
- 当前云环境的原生 GUI 启动限制仍存在，未把组件测试或 Web 构建当作真实桌面点击验证。
- 无合并、标签、公开发布、签名、购买服务或公网部署。
- MCP 当前不接受凭据引用；不支持 HTTP 传输、后台任务、自动 elicitation/sampling/MRTR、资源自动读取。
- Windows/Unix 的进程清理不是文件或网络沙箱，不承诺约束刻意逃逸的恶意程序；仅应配置受信任本机程序。

最终发布时记录冻结测试计数、远端 SHA/树、PR 与对应 CI；历史成功提交不能替代新提交验证。


## 首次 Windows 实际 CI 与修复

`c8780b9137aa5f95119fa48e15c2c984d0befd3c` 的 push/PR 两轮 Windows Rust 均观察到 **220 通过、1 失败**：固定根路径仍可被 rename。其余 Windows Job、取消/背压、双协议、环境、junction 拒绝等实际夹具通过，不能因此忽略失败项。根因是仅请求 `FILE_READ_ATTRIBUTES` 的句柄不建立预期的读访问共享约束。

修复将路径锁句柄访问改为 `GENERIC_READ`，仍不允许 share-delete/share-write，保留原失败断言和 nofollow/句柄生命周期；权限不足会安全拒绝，而不是放松检查。该两处源码 hunk 经独立静态复审，隔离 Linux MCP 10 项、Windows target 全目标 Clippy、格式检查通过。修复提交的实际 Windows CI 在发布后单独跟踪，不能把交叉编译当成已修复的运行证据。


## 精确提交平台收敛

最终修复提交 `7cd1b16c50a75985e4e7428a956da99c33ead0c0`（tree `ccfebdaae4fb58269ad3b5774b5b0ad323ca2221`）已通过 [push CI](https://github.com/YS1109/self-agent/actions/runs/40766271204) 与 [PR CI](https://github.com/YS1109/self-agent/actions/runs/40766281131)，两轮各 **6/6** 三平台 Rust / Desktop 检查成功。Windows daemon **221/221**、macOS **272/272**，全部 15 个后台 smoke 通过；目录锁的原始 rename 断言未弱化。

中间提交 `a9a64075607bab751a388d892291d63d03714594` 的目录锁测试已通过，但 Windows smoke 的 SQLite 句柄关闭与 macOS Python 启动夹具仍失败。最终仅修复测试路径解析、启动等待和数据库 context manager 显式关闭；实际工具调用超时/取消断言及生产语义未更改，父级独立复审通过。首次失败与中间失败不记作成功；最终 CI 也不替代后续命名服务切片验证。
