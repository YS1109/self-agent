# 1.0 校验/暂存、兼容与包夹具切片

状态：主程序/CLI/API/只读界面已接线，独立实际集成审查 clear，本地冻结门禁通过。精确提交/CI 待发布跟踪，不把本切片称为整个 1.0 完成。

实际结果与未运行项见 [acceptance-update-v1.0.md](acceptance-update-v1.0.md)：Rust core13/daemon683/CLI50/gateway17、前端331、Native纯源25、19后台smoke、更新4组、包布局4组、Web4组已通过。I/O所有权窄修后精确重建与更新/包夹具重跑通过；同一5,000任务seed副本实际schema1→4就绪及3次温启动均在未重置30秒截止内，保留20.335秒慢样本，不称数据库加速或GUI通过。完整本机Tauri构建因缺系统库受阻，由精确CI另验；没有实际GUI、OS安装器、生产签名或真实信任登记。

1. 扩展已审查签名 primitive，加入独立 owner verification-only trust，生产自动下载/安装继续禁用。
2. 隔离 SQLite update_state、作用域 floor 与不随 task backup 回滚的私有 witness；覆盖签名坏字节、错平台/schema、降级、配置撤销、CAS、领取后中断、旧备份恢复与不自动执行。
3. 在性能 schema v3 之后协调显式 v4，接真实 Store/daemon/owner API/CLI/状态界面，使用流式有界上传而非 artifact JSON 或任意路径执行。
4. 复用 lifecycle 兼容 helper 为 core 单一 wire 定义，先认证后协商，旧/新版本不兼容不得发送动作；drain 由另一生命周期切片完成。
5. 实际本地多进程与临时包布局夹具、独立审查、冻结门禁、精确 SHA CI。未运行生产签名/安装、真实 GUI、外部源或部署必须保留在验收记录。

接口细节见 [protocol-v15.md](protocol-v15.md)。该切片不自动完成整个 1.0 路线图。
