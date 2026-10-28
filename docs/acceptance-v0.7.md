# 0.7 验收跟踪

更新：2026-10-28。开发分支 `codex/v0.7-native-events` / 草稿 PR #1；用户本地审核合并，不自行 merge/tag/release。运行版本暂为 0.6.0。本表区分实现、夹具验证与真实环境验证，不把分切片结果冒充整版完成。

| 范围 | 当前实现/验证 | 仍未证明或待实现 |
| --- | --- | --- |
| Native 流式、完整工具参数、跨分片脱敏 | 已提交；本地 HTTP 夹具与三平台 CI | 真实模型按用户要求未调用 |
| 持久游标、断线回放、CLI 跟随 | 已提交；迁移/保留/重启/迟到终态回归 | 不承诺无限期事件审计 |
| Codex 事件、已知身份 thread 续接 | 已提交；实际 stable schema + stdio 夹具 | 托管/动态/未知身份仍明确新 thread 导入历史 |
| Native 跨任务摘要 | 已提交；三轮、重启、后缀/取消/策略失效 | 真实长上下文质量未测 |
| 桌面/浏览器事件桥 | 已提交；前端、真实 HTTP 代理、Rust bridge、三平台 CI | 云端 GUI/布局/缩放实测受系统库与沙箱限制 |
| 补充、排队、停止、继续 | 第三切片已实现；Native/Codex ACK、拒绝、终态竞态、断线、重启夹具 | 冻结本地门禁已通过；对应提交 CI 待运行；Applied 不是模型遵守保证 |
| 持久估算/压缩展示 | 已实现；本地估算与服务用量分离，事件淘汰/重开回归 | Codex 当前输入估算不是完整 thread 占用 |
| 项目执行区间 diff | 已实现；cap-std 有界 no-follow、失败/取消、锁后基线、隐私及持久化回归 | 不能把外部并发编辑独占归给 Agent |
| 项目全部 Git 状态 | **待安全实现**；当前明确不可用，index/config 保留 | git status 可执行 clean/process filters；不能绕过授权运行。单纯换 libgit2 也不能假设满足资源/no-follow 边界 |
| 联合 GUI 与正式 0.7 交付 | 尚未完成 | 应用版本提升、最终验证矩阵和已知边界需整合；没有发布授权 |

## 已发布检查点

- `bb0b7e08fa2d94f498c9b428206f8aebcab05273`：首切片，push 40721056695 / PR 40721141000 成功。
- `c013d74069de9f8f104dcd7ffcacbe4682cd6e32`：第二切片，首次跨平台 CI 发现 Windows 测试辅助 cfg 与 macOS 临时路径夹具问题。
- `b97d1f71a0edf46e6bf2370697f35800774d610d`：上述修复，push 40736034504 / PR 40736044231 两次各六项成功。

## 当前验证规则

默认只用本地 HTTP / stdio 模拟模型，工具操作发生在隔离临时项目。禁止读取用户认证文件、发送真实通讯消息、购买/部署服务、合并 main 或发布。GUI 失败/未运行如实记录，不以 Web 构建代替视觉验收。

新增 smoke：`steering_smoke.py`、`changes_smoke.py`；保留原八组及 streaming/continuation/web_stream。Codex 可执行夹具只在 Unix 运行，Windows 明确跳过并继续 Native 回归。第三切片冻结本地门禁：core 5、daemon 185、CLI 19、前端 104、同文件 Rust 事件桥无头测试 7 项；格式、全目标 Clippy、daemon/CLI/Web 构建通过。十二个后台 smoke 脚本全部通过，其中 streaming 8、continuation 8、steering 10、changes 7 组；HTTP 代理另有 3 组。对应提交三平台 CI 待运行，不能用上一切片成功代替。

已修复的本地门禁失败：取消夹具过早观察服务端标记，尚不能证明客户端取得 turn ID；改为同步实际客户端 ACK 进度后完整重跑通过。仍未运行/未验证：真实模型、当前版本原生 GUI、云端完整 Tauri 构建（缺 GTK/WebKit 开发库）；事件桥无头测试不是完整桌面构建。
