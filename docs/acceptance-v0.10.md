# 0.10 网关与移动 H5 · 验收记录

这是开发分支上的本地模拟验收，不是公网服务上线。daemon/桌面版本沿用现有版本；独立 gateway crate 的版本不代表已发布安装包。实现合同见 [protocol-v11.md](protocol-v11.md)。

## 实际接线

- 独立认证网关、daemon 出站 WebSocket supervisor、真实 Store 命令事务、系统凭据 intent/单次处理、配对/撤权、桌面 owner 设置和 CLI 已连接；H5 是独立同源构建，不导入本机管理令牌。
- schema v2 包含八个 gateway 表；旧 v1 先验证形状再原子升级。恢复禁用配置、解绑凭据引用、撤销浏览器权威、使旧执行回执不可重放；保留历史而不操作真实 vault。
- 结果仅授权拉取/SSE：gateway 来源任务不创建通用回调投递，不伪造 sent。任务终态、事件与会话消息仍同事务持久保存。

## 实际接线证据

| 范围 | 实际检查 | 状态 |
| --- | --- | --- |
| Store 集成 | 真 SQLite 的来源构造、命令与回执同事务、读写 ACL、输入审计、撤权/恢复、脱敏与资源界限 | 实际 Store 15 项通过，含无假回调回归 |
| 版本迁移/恢复 | v1→v2、DDL 失败回滚、必需列/唯一约束、实际备份后的 gateway 权威失效 | 纳入最终 daemon 全量门禁，含跨平台修复后的迁移与恢复回归 |
| 真实多进程 | 实际 daemon + 独立 gateway + 本地 Native HTTP 模型夹具，真实 HTTP/WS，不是 trait 假实现 | 最终安全修复后 5 组重跑通过 |
| 多进程覆盖 | 本机确认前拒绝、会话/CSRF/伪 source 拒绝；稳定 ID 单次执行与回执；问题回答不是审批；活动任务撤权及资源阻断；网关重启重新配对且不重放 | 仅临时测试凭据、loopback，无外部服务 |
| 前端 | 真实 React/jsdom mount 的初次读取、后续快照替换与关闭；H5 未知命令只读恢复/错回执保留未知、游标降低恢复、重复点击与迟到结果 | 最终全量 265 项通过 |
| 独立协议 | cookie/device key 隔离、Origin/CSRF、目录忙不等于撤权、迟到回包配对、最坏 JSON 转义限额、慢客户端和配额 | 主 workspace 服务 17 项、core wire 2 项及 daemon 模块回归通过 |

本地冻结门禁与独立源码审查已完成；精确远端提交及三平台 CI 在发布后单独记录，不把本地通过提前称为 CI 成功。

## 审查修复

- 失效游标恢复必须接受权威快照的较低 cursor，并使旧流回调失效。
- 设备目录校验暂时忙/不可读时拒绝本次操作，但不把它冒充已撤销并永久销毁有效 cookie。
- mutation 成功回执必须匹配原 command/session/status；缺失或错 ID 的 `ok` 仍未知，只能只读恢复。
- 超限 JSON 转义回复返回有界的原 request ID `outcome_unknown`，不使整个设备连接反复断开。
- `read_artifacts` 必须同时有 `read_results`；每次读取再次校验实时 ACL、所属 task/session 和当前脱敏。
- owner 面板初次 mount 不再用相同初始空快照中止刚开始的后台读取；新增实际 mount 回归。

## 尚未验证 / 有意排除

- headless Chromium 在当前执行环境启动被沙箱阻止，未取得手机截图；jsdom/SSR 不是浏览器视觉或原生 GUI 验收。
- 未部署 nginx、公网 WSS/TLS、域名、证书或服务器；未发真实 QQ 消息，未调用真实模型，未录入/读取真实 OS 凭据。
- 网关 browser session 仅内存，有效 cookie 内刷新/重连可恢复；网关重启需重新配对，不宣称持久登录。
- 产物仅有界持久纯文本，不提供工作区任意路径、二进制附件或下载。历史与资源容量达到上限时失败关闭，不自动删除不确定/审计记录。
- 设备目录依赖可信用户私有目录，不是防恶意同用户 ABA 的 SQLite capability VFS；有界限流不等于已验证公网抗拒绝服务。
- 已接受到独立授权桌面任务的补充/回答不随远程撤销回滚，也不转移任务来源；撤销关闭未来访问，不能借此获得未授权的取消权。

## 2026-10-29 最终本地冻结门禁

基线为 `76e76226067854d666b07566b069b6f19ad8c2ff`，包含已独立发布的存储跨平台修复。以下针对完整 0.10 接线，排除未接线的 1.0 lifecycle/update/performance 文件：

- Rust：core **9**、daemon **595**、CLI **37**、独立 gateway **17**（3 单元 + 14 真实 loopback）全部通过；fmt、全 targets Clippy `-D warnings`、三后台程序构建通过。
- 前端 **265 / 265**（33 文件），Web 与 mobile 正式构建通过。真实 React/jsdom mount 覆盖初读不被 abort、prop replacement 以及 Close 后迟到读取；它不是浏览器视觉验收。
- **18 个后台 smoke** 均用最终新编译、独立固定的真实 daemon/CLI/gateway 二进制通过，其中 gateway 5 组、reliability 4 组；旧 QQ/MCP/执行/续接/changes 门禁全部重跑。
- Web 流式代理 **3 组**，桌面桥精确路由提取 **4 项**（gateway/QQ/MCP/maintenance）通过；不是完整 Tauri GUI 构建。
- API **5 项**包含 current-thread Store 锁竞争时定时器仍推进、HTTP 观察断开后已接收凭据 intent 仍只写一次、原 ID 重放不再写，以及 8 项 admission 饱和即 429。SQLite 等待不占 Tokio 工作线程，已接收凭据工作持有额度至最终记录。
- 独立只读审查覆盖隔离服务与实际 Store/API/supervisor/restore/UI/CLI，已报告的修复复读 clear，无剩余源码 blocker。最终精确提交与远端 CI 另行跟踪；没有 merge、tag、release 或部署。

仅作格式/容量提示的 Web chunk-size 警告仍存在，不是构建失败。当前环境未安装 nginx，示例仅官方指令核对与源码审查，未运行 `nginx -t` 或真实 TLS 代理。

精确提交 `3160a4e2e0b0cf54030c56c43b50c6f8e136680c` 的 push `40792802861` / PR `40792812481` 均终态 6/6 成功（三平台 Rust 与 Desktop）。这不替代真实部署、系统凭据和手机视觉验证。
