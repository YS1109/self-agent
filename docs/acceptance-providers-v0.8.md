# 0.8 命名 Provider / 系统凭据验收

## 本切片内容

命名模型服务注册与选择、Native/显式编排固定服务身份、系统 vault 一次性设置/轮换/删除/孤立条目清理、客户端只读恢复、运行中撤权及秘密输出保护。协议见 [v9](protocol-v9.md)。v8 MCP/Skills 功能保留；MCP env 凭据仍未接线，QQ/H5 不属于本切片完成声明。

## 有行为价值的定向验证

- Provider URL/身份和 vault 类型与异步工作线程：22 项隔离单元测试曾通过；将在完整冻结门禁中重新运行。
- Store 固定任务/计划角色身份、禁用/地址变化/轮换的后续节点阻塞、已完成前驱与内部保存点、规划响应提交撤权 fence、未知 set/delete 清理保留原始事实，均纳入最终 daemon 门禁。
- 真实 HTTP owner API + `cfg(test)` memory vault：设置确认、同 ID 不同秘密不重放、拒绝/未知、重启不重放、客户端断开继续完成、删除先解除绑定、SQLite/WAL/事件/回执 canary；定向 5 项通过。
- Native loopback 模型 + memory vault：一个任务只读取一次凭据、固定模型选择、模型目录 canary、真实 read_file 工具与事件/报告脱敏、等待 HTTP 时撤权取消、系统 key 缺失无旧手动 key fallback；定向 3 项通过。
- 新增 Native 嵌套 JSON 历史脱敏回归：带引号和反斜杠的密钥先按语义值脱敏再嵌入工具消息。
- UI 审计记录 1025 / 1280 可恢复、1281 拒绝，cleanup_status 与原 unknown 分开显示；CLI 真实 stdin→HTTP 一次发送、网络不确定只读恢复。Codex 与 named Native 默认模型保持隔离。

## 未执行

- 没有读取或写入真实 Keychain、Credential Manager、Secret Service；平台库编译不代表真实解锁/授权验证。
- 没有真实模型调用、账号或机器人凭据登记、付费服务调用、QQ 真消息或公网部署。
- 云端 GUI 依赖/访问受限，未继续尝试桌面 GUI 验证；组件与同源桥测试不能替代实机视觉验收。

## 最终本地冻结门禁

2026-10-28 在排除未接线 QQ 文件的独立候选目录验证（实际代码 tree `e0a8503dc618b8d78fee344d1036a924c6b934c7`，随后仅补充本文结果）：

- core **7**、daemon **322**、CLI **27** 项通过；fmt、全目标 Clippy `-D warnings`、daemon / CLI 构建通过。
- 前端 **166** 项、TypeScript / Web 构建、Web 事件代理 **3** 组、同文件 Rust 事件桥 **7** 项通过。桥测试并非完整 Tauri 构建；完整平台构建由精确 CI 检查。
- **15 个后台 smoke 脚本**全部重新运行通过，包含此前的流式、续接、补充、改动、预算、交互、MCP / Skills。
- 独立只读审查通过；修复本节记录的权限、身份、秘密、审计和事务边界后，重新跑完上述全量门禁。
- Windows vault 的同源模块全目标交叉 Clippy 通过。完整 daemon Windows 交叉构建因本机缺少 MinGW C 编译器（SQLite C 构建）未完成，**不标记通过**；真实 Windows / macOS 的平台编译与测试须跟踪发布后 CI。
- 候选目录初次 pnpm 启动器试图自动安装到不可写默认 home，测试没有开始。改用已安装锁定依赖的直接 test/build 程序，并关闭该临时目录的自动安装检查后，Web 代理重新运行通过；未修改产品代码或下载替代依赖。
- Web 构建保留 Tauri 混合导入及主 JS 约 540 KiB 分块建议警告，退出成功；没有将警告隐瞒为无警告。

远端 commit / tree 与该精确提交 CI 在发布后分别核验；此前提交的 CI 不挪作本切片通过证据。
