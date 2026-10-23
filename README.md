# Self Agent

一个本地优先的 Harness Agent 桌面工作台。通过对话管理会话、派发异步 coding 任务，并逐步开放可编辑计划图、subagent 编排和远程通讯入口。

采用 **Rust daemon + Tauri 2 + React / TypeScript**。桌面以 Codex 风格的会话工作台组织项目、对话、任务卡片和侧边工作面板。

![桌面工作台](docs/screenshots/workbench.png)

## 当前版本

这是可运行的 v0 基础版本。已经接通：

- 独立 Rust 后台、本机令牌认证、SQLite 持久化会话与任务。
- 会话创建、切换、消息管理，以及不等待工作节点完成的任务派发。
- Chat Completions 兼容网关的原生 agent 工具循环，支持工作区文件读取、搜索、写入，以及显式启用的命令执行。
- 清楚标注的演示模式，在没有模型凭据时验证异步执行和通知链路。
- 固定任务回传目标、持久化通知队列、模拟远程入口和投递记录。
- React 桌面界面、任务详情和计划可视化，以及保持 daemon 独立运行的 Tauri 桥接。

v0 的主 Agent 负责确定性的会话管理和派发确认；自然语言自动分解、可编辑编排图、角色间智能调度仍按后续里程碑开发。Codex 执行器、外部 MCP / Skills、真实通讯平台、移动端尚未接通，界面会标明可用状态。

## 开发环境

- Rust **1.96 或更新版本**。
- Node.js **24**，项目使用的 pnpm 版本见 `package.json`。
- Tauri 平台构建依赖：macOS 需要 Xcode Command Line Tools；Windows 需要 MSVC C++ 构建工具及 WebView2；Linux 需要 WebKitGTK 4.1、GTK 和相关开发包。

安装前端依赖并启动桌面：

```sh
pnpm install
pnpm dev
```

启动脚本先构建 daemon、准备桌面 sidecar，再启动 Tauri 和 Vite。关闭桌面后 daemon 继续运行；任务与会话保存在平台本地应用数据目录的 `self-agent` 子目录中。

仅启动后台：

```sh
cargo run -p harness-daemon
```

仅启动浏览器开发界面时，先运行 daemon，再运行：

```sh
pnpm dev:web
```

默认开发地址为 `http://127.0.0.1:1420`。Vite 代理从本机私有元数据读取后台令牌，前端构建产物不包含该令牌。开发服务器只应监听本机。

## 配置模型

通过设置界面填写兼容 Chat Completions 的网关地址、模型与 API Key。v0 在设置页输入的密钥只保存在 daemon 内存中；重启后需要重新输入，或改用环境变量。

```sh
export HARNESS_BASE_URL=https://api.openai.com/v1
export HARNESS_MODEL=gpt-4.1
export HARNESS_API_KEY=your-key
pnpm dev
```

`HARNESS_BASE_URL` 指向包含 `/v1` 的 API 根地址，不包含 `/chat/completions`。项目不会自动加载 `.env` 文件。更多配置见 [.env.example](.env.example)。

没有模型凭据时，可以在设置中显式切换为演示模式。演示任务只产生模拟阶段和通知记录，不修改项目、不声称通过真实测试。

文件工具使用项目相对路径。命令执行默认关闭，启用方式：

```sh
export HARNESS_ALLOW_COMMANDS=1
pnpm dev
```

启用后，模型运行的命令拥有当前用户的本机权限；工作目录限制不能代替操作系统沙箱。先在适合试验的项目中使用。原生循环目前限制为 10 个模型轮次。

## 远程入口与回传

桌面中的远程模拟器使用独立的会话选择上下文。任务派发时保存回传目标，之后切换会话不会改变旧任务的归属。

子任务终态、结果事件和通知队列在同一 SQLite 事务中提交。独立投递 worker 处理通知；投递状态与任务执行状态分别保存。目前 desktop / simulator 是本地持久化接收端，尚未向任何通讯平台发送消息。

daemon 默认只监听 `127.0.0.1:47371`。未来通讯工具与移动端通过统一指令协议接入，本版本不开放公网控制端口。

## 验证与构建

```sh
cargo fmt --all -- --check
cargo test -p harness-core -p harness-daemon
cargo clippy -p harness-core -p harness-daemon --all-targets -- -D warnings
cargo build -p harness-daemon
python3 scripts/smoke.py
python3 scripts/provider_smoke.py
pnpm test
pnpm build:web
```

两个 HTTP smoke 使用独立临时目录与本地端口，覆盖鉴权、输入去重、执行期间管理消息、回传目标、取消、重启，以及真实文件工具调用。原生模型测试通过本地确定性 HTTP fixture 完成，不消耗云端模型额度。

构建当前平台桌面安装包：

```sh
pnpm desktop:build
```

此命令先构建 release daemon，再准备对应 Rust target 的 sidecar，并调用 Tauri 构建。跨平台构建由三端 CI 验证；正式分发签名、自动更新与安装包发布另行配置。

## 目录

```text
crates/harness-core/       共享协议与类型
crates/harness-daemon/     HTTP、SQLite、调度、原生执行与通知
src-tauri/                桌面生命周期和受限 API 桥
src/                      React 会话工作台
scripts/                  开发、sidecar 和端到端验证
docs/                     产品计划和当前协议
```

完整设计见 [产品与架构计划](docs/architecture-plan.md)，当前实现协议见 [v0 integration contract](docs/protocol-v0.md)。开发进度见 [实施状态](docs/implementation-status.md)。
