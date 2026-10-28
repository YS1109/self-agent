# 0.8 MCP 环境凭据验收

此切片将系统 vault 接到真实托管 stdio MCP，而非只增加配置输入框。协议见 v9 附节。本地冻结候选已执行下列门禁；精确提交的三平台 CI 单独跟踪，不沿用上一切片结果。

## 已执行的实际路径

- 主仓独立 Store **14** 项：作用域、完整配置、CAS、一次 claim、Unknown 恢复、孤立清理、全部槽要求、目录失效、快照丢失/重复、legacy ref 隔离、配额及原子 detach。
- 真实本机 owner HTTP → memory vault → Native loopback 模型 → 真实 Python stdio：两次 MCP 工具调用每槽只读一次；秘密含 UTF-8、引号、反斜杠和换行，text 中的 JSON 键/值及嵌套 JSON string 进入模型前已处理。事件、快照、SQLite/WAL、报告不包含该秘密。
- 专用 API **6** 项覆盖未知 set/delete 只读恢复及清理原始事实、metadata 的 JSON 文本编码秘密拒绝、owner 认证、危险声明及未声明槽、读取失败无 Provider fallback、vault 读取中撤权不发模型/不开进程。
- 共享 JSON redactor **3/3**、跨流片段编码 **1/1**、晚发现秘密的报告过滤 **1/1** 通过；20 万短字段 / 64 长密钥包含在 3 项中，测试组本机实测 0.50 秒。旧 raw-only 实现漏掉标准 JSON text，独立审查发现后修复。联合 API 与数据库断言额外禁止稳定 canary 前缀，不依赖待测 redactor 作为验证依据。
- UI / CLI 已接独立专用路由，密码不保存到 React 状态、argv 或日志；未知不重发、仅 GET 恢复，原 unknown 与 cleanup_status 分开。最终前端数字以排除 QQ 的冻结候选为准。

## 未验证与已知边界

没有真实 OS vault 授权、真实第三方 MCP、模型费用或 GUI 点击；所有秘密均为本地夹具。测试成功不证明任意可信程序会正确使用环境变量，也不把进程清理称为文件/网络沙箱。语义处理仅覆盖有界 JSON 文本和常见 JSON 表示，不泛化为任意编码解码。

## 独立冻结门禁

- 排除 QQ 与 1.0 未接线模块的候选：core **7**、daemon **347**、CLI **29**、前端 **182** 全过；fmt、全目标 Clippy `-D warnings`、daemon/CLI 构建与 Web 构建通过。CLI 帮助文本修订后重新跑完 29 项及 fmt。
- 同源 Rust 事件桥 **7** 项、MCP 桌面路由纯逻辑提取 **1** 项通过；这不是完整 Tauri/GUI 验证。Web 事件代理 **3** 组通过。
- 15 个既有 backend smoke 全部通过（包含真实本机 HTTP/SSE/stdio、SQLite 重启及 CLI 路径，无外部模型）。精确 SHA 三平台 CI 在发布后单独跟踪，当前尚未运行。
- Web 构建保留大于 500 kB chunk 的体积警告，无构建错误；尚未声称完成资源优化。
- 独立只读审查无剩余具体阻塞；审查未运行测试，门禁由实施者另行执行。

QQ 和 1.0 新模块不属于此切片。
