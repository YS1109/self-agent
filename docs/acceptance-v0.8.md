# 0.8 执行控制切片验收记录

日期：2026-10-28。范围仅为预算、Native 修复/复验、权限、问题及逐次授权；不是完整 0.8。全部测试使用隔离项目、loopback 模型或 stdio 协议夹具；真实模型、真实凭据、当前 GUI 和发布均未验证。

## 已验证的安全边界

| 边界 | 实际检查与结果 | 主要入口 |
| --- | --- | --- |
| 固化预算与权限 | 设置/角色/本次/继续来源取交集；部分设置补丁不清除原限制；非法范围与 Codex 不支持维度拒绝 | `store/execution.rs`、`lib.rs` 单元测试 |
| 模型与输出预算 | 模型次数包含压缩/安全重试；10KiB 实际 shell 输出和验收输出在 1024 字节额度下不能经 evidence/verification 绕过，退出码保留、超额文本不保存 | `budget_smoke.py` |
| Codex 输出证据 | 证据与相邻工具事件先统一预留最长表示，重复表示不重复计量；无对应事件的记录独立限额 | `execution_control/tests.rs`、`continuation_smoke.py` |
| 时长与进程清理 | Native 在途模型/普通 shell 子进程停止；Codex version/config 预检取消，确认进程退出且没有 thread/turn 启动 | `budget_smoke.py`、`continuation_smoke.py`、Codex 单元测试 |
| 阻塞文件写入 | 原子替换前确定性门控，取消/超时后 provider 不返回、同项目 semaphore 不释放；放行后保存真实写入和 settled，不产生第二回调 | `provider/execution_tests.rs`；测试直接调用实际 provider 并持有与 run_task 相同项目 semaphore，未向生产路径加入测试暂停端点 |
| 重启清理不确定性 | pending → unknown，不宣称 settled、不重放；取消/恢复原因先到先保留，事务失败回滚 | `store/execution.rs` |
| 项目范围 | 保存的根被替换为外部符号链接时，在模型启动前拒绝；固定目录句柄抵抗执行中路径替换，拒绝链接和特殊文件，写入不截断共享硬链接 inode | `project_files.rs`、`budget_smoke.py` |
| 读取权限 | read_files=false 不采集项目内容/名称报告，持久保存 unavailable；撤销后不公开结束报告 | `budget_smoke.py`、Rust 集成代码复审 |
| 实际修复/复验 | 失败 → 写入修复 → 原命令批次复验；原失败仍在，旧修改代次不能满足新验收 | `budget_smoke.py` |
| 问题/授权 | 类型、revision、digest、任务、过期、撤权、取消和重启检查；真实 CLI 决定只发送一次 POST，随后只读 follow；重复信封不重写 | `store/decisions.rs`、`execution_control_smoke.py` |
| 桌面与 CLI | UI 的 16000 UTF-8 字节边界、Codex 不支持维度、角色继承、清理警告；CLI 参数/stdin/退出码/清理提示 | 前端 124 项、CLI 23 项检查通过，Web 构建通过 |

## 总体门禁

最终本地冻结门禁通过：core 7、daemon 241、CLI 23、前端 124 项；格式、全目标 Clippy `-D warnings`、daemon/CLI/Web 构建通过。十四个后台 smoke 脚本均通过，其中 budget 13 组、execution_control 14 组、continuation 10 组、steering 10 组；Web 事件代理 3 组、同文件 Rust 事件桥无头测试 7 项通过。0.7 身份实现最终保持严格 managed fresh fallback；其源码收敛后重跑 Rust 全套以及受影响的预算、续接、补充、角色、配置脚本，结果全部通过。

本轮完整检查曾发现交互夹具用空对象清除禁写权限的旧假设；生产部分补丁按设计保留限制。夹具增加空补丁保留断言并显式恢复所有者允许设置后，完整 14 组通过。新夹具对预期取消断连同时处理 Windows ConnectionAbortedError，进程存在检查在 Windows 使用只读 tasklist，不用会改变进程状态的 os.kill。

以上是本地模拟门禁，不是精确提交 CI。提交/远端树验证及 CI 结果在发布后单独跟踪；当前未验证真实 GUI 或打包安装。

## 保留的限制

- shell 仍需显式 `HARNESS_ALLOW_COMMANDS=1`，是可信本机命令，不是项目或网络沙箱；不声称能约束恶意脱离进程组的进程。Codex 的 cwd 每次启动前检查，但不是可移植的 capability-pinned cwd。
- 时长到期阻止新工作，不代表不可中断的内核写入能在同一刻结束；取消不回滚。unknown 需要检查实际文件。
- 输出预算针对回复/逻辑工具结果，不是磁盘总量、模型 token、费用或独立改动报告预算。
- 验收批次代次描述执行顺序，不保证脚本只读，也不证明外部编辑器没有改动。
- MCP / Skills 应用管理、命名服务与系统凭据生命周期仍待后续切片。跨平台 CI 和 GUI 的未执行项不能由当前 Linux 模拟门禁替代。Codex stdio 夹具当前在 Unix 运行；Windows CI 中的明确跳过不算该执行器路径实测。

## 首次跨平台 CI 与夹具修复

`970fec2` 的 push/PR 两次运行中，Linux Rust 与三平台 Desktop 均通过。macOS 的旧 Native 单测直接传入系统 `/var` 临时目录别名，被新 no-follow 执行策略正确拒绝；修复仅在测试选择阶段 canonicalize，与会话创建一致。实际 Unix socket 夹具缩短随机目录名，避免 Darwin 路径长度上限。Windows 的 hook 单测在删除临时目录前显式释放 capability 句柄。另一个 macOS run 的 Codex 测试外层 6 秒总期限不足（另一 run 通过）；仅将测试框架总期限增到 15 秒，保留全部 ACK/次数/不重放断言，生产时长/取消规则不变。修复提交的精确 CI 继续单独核验，不把首次失败记为通过。
