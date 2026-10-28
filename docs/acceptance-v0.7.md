# 0.7 验收跟踪

更新：2026-10-28。开发分支 `codex/v0.7-native-events` / 草稿 PR #1；用户本地审核合并，不自行 merge/tag/release。运行版本暂为 0.6.0。本表区分实现、夹具验证与真实环境验证，不把分切片结果冒充整版完成。

| 范围 | 当前实现/验证 | 仍未证明或待实现 |
| --- | --- | --- |
| Native 流式、完整工具参数、跨分片脱敏 | 已提交；本地 HTTP 夹具与三平台 CI | 真实模型按用户要求未调用 |
| 持久游标、断线回放、CLI 跟随 | 已提交；迁移/保留/重启/迟到终态回归 | 不承诺无限期事件审计 |
| Codex 事件、已知身份 thread 续接 | 已提交；实际 stable schema + stdio 夹具 | 托管/动态/未知身份仍明确新 thread 导入历史 |
| Native 跨任务摘要 | 已提交；三轮、重启、后缀/取消/策略失效 | 真实长上下文质量未测 |
| 桌面/浏览器事件桥 | 已提交；前端、真实 HTTP 代理、Rust bridge、三平台 CI | 云端 GUI/布局/缩放实测受系统库与沙箱限制 |
| 补充、排队、停止、继续 | 第三切片已实现；Native/Codex ACK、拒绝、终态竞态、断线、重启夹具 | 冻结本地门禁及后续修复提交三平台 CI 已通过；Applied 不是模型遵守保证 |
| 持久估算/压缩展示 | 已实现；本地估算与服务用量分离，事件淘汰/重开回归 | Codex 当前输入估算不是完整 thread 占用 |
| 项目执行区间 diff | 已实现；cap-std 有界 no-follow、失败/取消、锁后基线、隐私及持久化回归 | 不能把外部并发编辑独占归给 Agent |
| Git 元数据概览 | 第四切片工作树已实现有界 no-follow index/HEAD/pack 读取，静态复审通过；不启动任何 Git 子进程 | 刻意 partial：工作区仅原始字节，转换/忽略语义未知；SHA-256、split/sparse、外部/替换 refs 等拒绝，不能声称全部 Git 状态 |
| 联合 GUI 与正式 0.7 交付 | 尚未完成 | 应用版本提升、最终验证矩阵和已知边界需整合；没有发布授权 |

## 已发布检查点

- `bb0b7e08fa2d94f498c9b428206f8aebcab05273`：首切片，push 40721056695 / PR 40721141000 成功。
- `c013d74069de9f8f104dcd7ffcacbe4682cd6e32`：第二切片，首次跨平台 CI 发现 Windows 测试辅助 cfg 与 macOS 临时路径夹具问题。
- `b97d1f71a0edf46e6bf2370697f35800774d610d`：上述修复，push 40736034504 / PR 40736044231 两次各六项成功。

## 当前验证规则

默认只用本地 HTTP / stdio 模拟模型，工具操作发生在隔离临时项目。禁止读取用户认证文件、发送真实通讯消息、购买/部署服务、合并 main 或发布。GUI 失败/未运行如实记录，不以 Web 构建代替视觉验收。

新增 smoke：`steering_smoke.py`、`changes_smoke.py`；保留原八组及 streaming/continuation/web_stream。Codex 可执行夹具只在 Unix 运行，Windows 明确跳过并继续 Native 回归。第三切片冻结本地门禁：core 5、daemon 185、CLI 19、前端 104、同文件 Rust 事件桥无头测试 7 项；格式、全目标 Clippy、daemon/CLI/Web 构建通过。十二个后台 smoke 脚本全部通过，其中 streaming 8、continuation 8、steering 10、changes 7 组；HTTP 代理另有 3 组。对应提交的跨平台修复和结果见下文，不能用上一切片成功代替。

已修复的本地门禁失败：取消夹具过早观察服务端标记，尚不能证明客户端取得 turn ID；改为同步实际客户端 ACK 进度后完整重跑通过。仍未运行/未验证：真实模型、当前版本原生 GUI、云端完整 Tauri 构建（缺 GTK/WebKit 开发库）；事件桥无头测试不是完整桌面构建。

## 第三切片跨平台修复

第三切片 `4d77bab2d8f000bde2b4ab8d844c09f5721f881d` 的 push 40743839739 / PR 40743844899 均在 Windows 新 changes smoke 发现夹具换行差异：Python write_text 默认生成 CRLF，而 Native 实际写入 LF，正确的区间 diff 因此包含先前行的换行变化。夹具现显式使用 LF，字节长度与预期跨平台一致；不修改生产 diff，也不隐藏真实换行变化。隔离该提交的独立工作树构建及七组 changes smoke 重跑通过。后续取消连接夹具修复与最终三平台结果见下文。

## 第四切片：安全 Git 元数据读取

不把 `git status` 当作无副作用读取，也不直接用缺少严格资源/no-follow 边界的 libgit2 statuses。新增本地有界解析器；格式依据 [Git index](https://git-scm.com/docs/gitformat-index)、[pack](https://git-scm.com/docs/gitformat-pack) 与 [loose object](https://git-scm.com/docs/gitformat-loose) 官方说明。

独立审查已修复并复核：loose ref 安全失败不得回退旧 packed 值；packed refs 必须完整验证才证明 unborn；重复/失序树条目不得合成虚假 HEAD；冲突及遗漏文件不推断原始工作区状态；commondir/reftable/replacement refs 拒绝。另修复选定根目录重新 canonicalize 的链接替换风险：逐组件 no-follow 打开并在整个区间使用同一目录句柄，覆盖根/祖先链接拒绝及中途替换回归。界面和持久化 schema 已区分 staged 元数据与 raw bytes；原始内容和私有凭据不新增持久化。

第四切片隔离冻结门禁通过：core 5、daemon 195、CLI 19、前端 105 项；格式、全目标 Clippy、daemon/CLI/Web 构建和十二个后台 smoke 脚本通过。隔离工作树的前端直接使用已安装的 Vitest/tsc/Vite，与 package scripts 等价；pnpm 的自动依赖状态检查试图写不可用的 home 缓存，未继续该安装动作。独立静态复审无剩余阻塞项。真实模型和 GUI 未运行，跨平台 CI 按本切片提交另记；不称整版已完成。

第三切片修复 `365942c3` 的 Windows smoke 已通过换行断言，随后发现取消连接以 ConnectionAbortedError（10053）结束的预期差异；`7142474c89e33e4c8a0b6fa473b5dcf9b1182708` 单行补齐该异常类别，意外异常仍记录失败。该精确提交的 push `40746966975` / PR `40746977940` 均已成功，两次各六项三平台检查；第四切片仍须自己的 CI。

第四切片 `83603bc8` 的三平台 Rust 门禁在 Rust 1.99.1 新增 Clippy 检查 `chunks_exact_to_as_chunks` 处失败（本地工具链 1.96 未报此 lint）。修复改用相同 20 字节固定数组分块，不放宽长度/顺序校验；独立格式、Clippy 与 24 项改动/Git 回归重跑后，修复提交 CI 另行跟踪。

修复 `6b7537fd` 已越过三平台 Clippy；PR Windows 的 steering smoke 随后出现预期取消连接的同类 10053，streaming fixture 也曾输出该异常。两者补齐 ConnectionAbortedError，只忽略已知断连类别，直接注入未知异常仍失败。另一次 PR Windows Web 构建在产物生成后发生 Node/libuv 关闭断言，而同 SHA push Windows 桌面成功；记录为待新提交 CI 核验的间歇工具问题，未伪记通过。
