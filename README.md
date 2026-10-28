# Self Agent

一个本地优先的 Harness Agent 工作台。日常像 Codex 一样与主智能体对话，直接读取项目、修改与验证；远程入口支持会话管理、子智能体编排及结果回调。桌面、无头 HTTP 和命令行共用 Rust 后台。

采用 **Rust daemon + Tauri 2 + React / TypeScript**。桌面以 Codex 风格的会话工作台组织项目、对话、任务卡片和侧边工作面板。

![桌面工作台](docs/screenshots/workbench.png)

## 当前版本

**0.7 开发分支（未发布）**：已打通 Native/Codex 统一执行事件、桌面/浏览器流式消费、持久游标恢复及 CLI 跟随，新增 Codex 主 thread 续接与 Native 跨任务摘要。详见 [事件与会话协议](docs/protocol-v6.md)。第三切片增加执行中补充、明确排队/继续、持久上下文估算和有界只读区间 diff。第四切片增加不启动 Git 子进程的有界元数据概览，区分 HEAD/index 与原始工作区字节；不冒充完整 Git 转换/忽略语义。托管登录原生续接仍有安全兼容边界，运行版本号暂不变。

当前版本 **0.6.0** 增加主智能体直接执行、可调界面缩放、自定义模型 Endpoint、模型上下文与子角色覆盖，以及无头命令行客户端：

- 桌面默认直接进入主 Agent 工具循环；同会话依次执行，带入前序对话与真实结果。
- 本机界面缩放 80%–120%，默认 95%，重启保留。
- 可配置服务根 Endpoint 与独立模型列表路径，实际获取模型后下拉选择。
- 按模型保存上下文窗口和压缩阈值，子智能体可逐项覆盖；原生执行器实际总结较早记录，Codex 使用自身压缩配置。
- 独立 `self-agent-cli` 支持创建和选择会话、直接执行、远程编排、状态、取消、配置、模型列表、完整管理指令与终态等待。

- 独立 Rust 后台、本机令牌认证、SQLite 持久化会话与任务。
- 模型驱动的直接执行与远程编排：接收立即返回，耗时执行期间继续接收结构化管理指令。
- 可编辑的草稿任务、描述和依赖，版本检查、循环依赖验证，以及真实依赖图。
- 审阅后启动计划，按依赖调度子任务，将前置结果传给后续任务；主智能体回顾后完成、追加工作或请求人工介入。
- 每个计划最多 3 轮、累计 12 个节点，独立任务 worker 与主智能体 worker；同项目的工具执行保持单写。
- Chat Completions 兼容网关的原生 agent 工具循环，支持工作区文件读取、搜索、写入，以及显式启用的命令执行。
- 清楚标注的演示模式，在没有模型凭据时验证异步执行和通知链路。
- 固定任务回传目标、持久化通知队列、模拟远程入口和投递记录。
- 中文 React 桌面界面、中性灰与蓝色明暗主题、任务详情和计划可视化，以及保持 daemon 独立运行的 Tauri 桥接。
- 本机 Codex app-server 执行器，独立任务会话、真实终态检查、取消中断与进程回收。
- 可编辑角色名称、职责、执行器和模型；每个草稿节点选择执行角色，配置随计划和任务固化。
- 原生窗口标题栏随明暗主题同步，修正顶部栏与工作区背景失配。
- 运行中调整未开始的节点、追加任务、修改角色与依赖；暂停停止新派发，在途任务可完成，恢复后继续。
- 实际命令与退出码证据、显式修复／验收关系；只有匹配命令的最终执行成功才能通过验收。
- macOS 顶部应用标题跟随界面明暗配色；空白区域屏蔽 WebView 默认右键菜单，保留输入编辑和选中文字的菜单。
- 可编辑 DAG 画布：选择节点查看和编辑详情、拖动布局、拉线设置依赖、删除连线、增删未开始任务；支持展开画布与返回对话。
- 自然语言补充当前计划，异步生成持久化调整建议，预览增删改后显式应用；版本过期时保留原文并重新生成。
- 桌面和远程模拟入口共用计划指令，各自选择目标会话与计划；布局使用独立版本，移动节点不影响执行或建议版本。

远程编排的计划先形成草稿供你审阅，启动后在预算内自动继续。执行中可以调整未开始的节点，已有运行及终态记录保留。失败、取消或阻塞节点需要关联修复与验收任务，并取得实际成功命令证据后，整个计划才允许完成；失败历史仍保留。建议先暂停派发、保存后续调整，再恢复执行。

外部 MCP / Skills 的应用内管理、真实通讯平台和移动端尚未接通。本开发分支 Native 主任务与原生子任务使用流式 Chat Completions（同一响应可兼容 JSON，不重试）；远程规划与回顾仍为非流式；Codex 主任务和子任务通过独立 stdio 协议运行。

## 后续规划与接手

下一阶段为 **0.7：持续会话与实时执行体验**，核心是流式输出与事件恢复、Codex 会话续接及原生摘要复用，随后完善执行中补充要求和代码改动查看。本开发分支已实现事件、会话复用、补充控制与区间改动切片，剩余安全兼容边界及联合验收继续推进，完整 0.7 尚未发布。

完整的 0.7 至 1.0 分版目标、依赖与验收见 [分版路线图](docs/roadmap.md)。接手开发先读 [0.7 交接记录](docs/handoff-v0.7.md)，再按 [0.7 迭代计划](docs/iteration-v0.7.md) 推进。后续自主执行、MCP / Skills、真实通讯、H5、候选 App 与稳定发布均已分开记录；平台和上线日期未定的项目明确标注待确认。

## 使用流程

1. 新建会话并选择已有项目目录，在设置中选择主执行器、模型服务及模型，或明确启用演示模式。
2. 日常在主对话输入目标并发送，主智能体直接调用项目工具执行，无需先生成和启动计划；AI 左侧、用户右侧显示。
3. 在设置里调整本机界面缩放，配置当前模型的上下文窗口和压缩阈值；子角色可单独覆盖。
4. 需要远程编排时，使用「远程实验室」或 API / CLI 的 `send_message`，后台形成计划草稿。可在计划面板及画布编辑、启动、暂停、调整和验收，完成记录固定回到最初来源。
5. 不打开桌面也可独立运行 daemon，再通过 HTTP 或 `self-agent-cli` 操作相同会话与任务。详见 [主智能体与无头协议](docs/protocol-v5.md)。

## Codex 与子智能体角色

在设置页「子智能体模型预设」中添加角色，为每个角色选择模型、执行器和职责，再选择默认角色。新角色默认固定当前主模型，也可选择跟随主模型；固定后切换主模型不会改变该角色的预设。计划草稿中可逐节点选择角色，任务和计划详情会展示保存的模型预设。主 Agent 的日常执行器独立选择 native 或 codex；远程编排的规划与回顾仍使用兼容模型。只用 Codex 直接执行时，可以不配置原生模型服务。

Codex 需要先在本机安装、完成登录并配置可用模型。在设置中填写 Codex 命令名或绝对路径，保存后点击检测。检测只运行 `--version`，不验证登录或调用模型。原生角色的模型留空时继承主 Agent 模型；Codex 角色留空时使用本机 Codex 配置。GUI 找不到 CLI 时请填写绝对路径，执行器会为子进程补充该文件所在目录的 PATH。

角色配置在发送目标时随编排作业保存；草稿、自动追加节点和延迟派发任务均使用该计划的角色快照。之后修改设置不会改变旧计划。当前最多 16 个角色；直接任务也会保存其执行角色。

桌面提供 80%–120% 本机界面缩放，AI 消息在左、用户消息在右，统一灰蓝主题的下拉菜单支持键盘操作。输入框和选中文字的右键编辑菜单使用中文；空白区域不显示浏览器菜单。

Codex 主任务在可确认的自定义服务身份、配置、历史和前次成功终态均匹配时复用持久 thread，通过 `thread/resume` 续接；新 thread 只引导应用历史一次。子任务仍使用独立临时会话。未知在途写入、活动 thread 或权限不符不自动重试；只有真实完成状态和最终结果才报告成功。公开 account/read 缺少不可变用户标识；实验工作区 ID 也不能替代 principal。因此 ChatGPT 登录或身份无法可靠绑定时明确新建 thread 并导入完整历史，不冒充原生续接，也不读取认证文件填补缺口。没有人工审批交互。执行权限为 `workspace-write`，禁止命令网络和额外临时目录写入，额外授权请求会被拒绝并终止。它仍继承本机 Codex 的登录和工具配置；第三方 MCP 的远端操作不属于本机文件沙箱的保障范围。

Codex 单任务最多 15 分钟、协议总输出 8 MiB、最终结果 128 KiB；取消时先中断回合，再回收执行器进程及其子进程。本分支依据本机 `codex-cli 0.166.0-alpha.1` stable schema 和本地夹具验证；未新增真实模型测试。0.5 在隔离项目完成真实主模型规划与对话修改、真实 Codex「修改→测试失败→修复→复验→回调」联合验收；这是单个受控案例，不代表任意任务的规划质量。

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

仅启动后台，无需桌面：

```sh
cargo run -p harness-daemon
```

另一终端使用命令行客户端（或构建后使用 `target/debug/self-agent-cli`）：

```sh
cargo run -p harness-cli -- create "开发会话" /absolute/path/to/project
cargo run -p harness-cli -- sessions
cargo run -p harness-cli -- --wait 600 run SESSION_ID "检查项目并完成修改"
cargo run -p harness-cli -- status TASK_ID
```

仅启动浏览器开发界面时，先运行 daemon，再运行：

```sh
pnpm dev:web
```

默认开发地址为 `http://127.0.0.1:1420`。Vite 代理从本机私有元数据读取后台令牌，前端构建产物不包含该令牌。开发服务器只应监听本机。

## 配置模型

通过设置界面填写兼容 Chat Completions 的自定义 Endpoint、模型与 API Key。模型列表路径可独立配置，留空使用 `/models`，可带查询参数但必须与服务同源；实际发现后下拉选择。直接主智能体和原生子智能体需要工具调用，远程编排需要 JSON 对象输出。手动输入的密钥只保存在 daemon 内存中；重启后需要重新输入，或改用环境变量。

也可选择「复用本机 Codex 服务配置」：每次请求从 `CODEX_HOME/config.toml`（默认 `~/.codex/config.toml`）读取顶层选中的自定义服务、模型、Bearer 凭据与静态／环境请求头。凭据不会复制到本应用数据库或前端，重启后继续读取源配置。当前不合并 profile、项目配置或 `auth.json`，不复用专属登录；这类情况请使用 Codex 子任务执行器。此模式仍调用 Chat Completions，源服务需要同时支持该接口，不能仅凭 Codex 的 `wire_api` 判断兼容。

「检测模型连接」对当前填写的配置发起一次简短 JSON 请求，产生少量模型用量，不保存草稿、不创建任务。模型列表可用与配置完整不代表推理成功；请以实际连接检测结果为准。模型字段拒绝疑似密钥，旧配置中的疑似密钥会自动清除；切换手动服务地址时会清除旧服务的内存密钥。

```sh
export HARNESS_BASE_URL=https://api.openai.com/v1
export HARNESS_MODEL=gpt-4.1
export HARNESS_API_KEY=your-key
pnpm dev
```

`HARNESS_BASE_URL` 指向包含 `/v1` 的 API 根地址，不包含 `/chat/completions`。项目不会自动加载 `.env` 文件。更多配置见 [.env.example](.env.example)。

没有模型凭据时，可以在设置中显式切换为演示模式。直接执行只产生明确标记的模拟步骤和通知；远程编排生成固定中文演示草稿。不修改项目，不声称通过真实测试。

文件工具使用项目相对路径。命令执行默认关闭，启用方式：

```sh
export HARNESS_ALLOW_COMMANDS=1
pnpm dev
```

启用后，模型运行的命令拥有当前用户的本机权限；工作目录限制不能代替操作系统沙箱。先在适合试验的项目中使用。每个原生子任务目前限制为 10 个模型轮次；计划最多 3 轮、累计 12 个子任务。主智能体单次请求限制为 120 秒。

## 远程入口与回传

桌面中的远程模拟器使用独立的会话选择上下文，可选择计划、发送补充要求、查询进度和暂停／恢复。任务派发时保存回传目标，之后切换会话或从另一入口修改计划不会改变旧任务的归属。

子任务终态、结果事件和通知队列在同一 SQLite 事务中提交。每个子任务与整个计划的最终父任务分别回传，并固定使用计划最初的发起来源。独立投递 worker 处理通知；投递状态与任务执行状态分别保存。目前 desktop / simulator 是本地持久化接收端，尚未向任何通讯平台发送消息。

后台重启会重试未提交结果的主智能体请求；正在执行的项目任务会标为失败并保留已有修改，不会自动重放写入。父计划继续推进到回顾与待确认状态。取消计划后，迟到的模型响应不能恢复计划。

daemon 默认只监听 `127.0.0.1:47371`。未来通讯工具与移动端通过统一指令协议接入，本版本不开放公网控制端口。

## 验证与构建

```sh
cargo fmt --all -- --check
cargo test -p harness-core -p harness-daemon -p harness-cli
cargo clippy -p harness-core -p harness-daemon -p harness-cli --all-targets -- -D warnings
cargo build -p harness-daemon -p harness-cli
python3 scripts/smoke.py
python3 scripts/provider_smoke.py
python3 scripts/configuration_smoke.py
python3 scripts/orchestration_smoke.py
python3 scripts/agents_smoke.py
python3 scripts/runtime_smoke.py
python3 scripts/plan_change_smoke.py
python3 scripts/headless_smoke.py
python3 scripts/streaming_smoke.py
python3 scripts/continuation_smoke.py
python3 scripts/steering_smoke.py
python3 scripts/changes_smoke.py
python3 scripts/web_stream_smoke.py
pnpm test
pnpm build:web
```

原八组 HTTP / CLI smoke 和新增流式、续接及代理 smoke 使用独立临时目录与本地端口，覆盖鉴权、指令去重、执行期间管理消息、回传目标、取消、重启、真实文件与命令、计划版本冲突、DAG、依赖结果、追加任务、预算、角色快照、混合执行器、暂停恢复、修复验收、持久化调整建议和独立布局版本。默认采用本地确定性 HTTP / stdio fixture，不消耗云端模型额度。可额外运行 `python3 scripts/runtime_smoke.py --real-codex` 验证真实执行器，或显式配置 `HARNESS_BASE_URL / HARNESS_MODEL / HARNESS_API_KEY` 后运行 `python3 scripts/plan_change_smoke.py --live` 验证真实主模型与 Codex 的联合闭环；这些选项会调用已配置模型。Codex 可执行夹具仅在 Unix 运行，Windows 使用原生角色完成 HTTP 链路验证。

构建当前平台桌面安装包：

```sh
pnpm desktop:build
```

此命令先构建 release daemon，再准备对应 Rust target 的 sidecar，并调用 Tauri 构建。跨平台构建由三端 CI 验证；正式分发签名、自动更新与安装包发布另行配置。

## 目录

```text
crates/harness-core/       共享协议与类型
crates/harness-daemon/     HTTP、SQLite、调度、原生执行与通知
crates/harness-cli/        独立无头命令行客户端
src-tauri/                桌面生命周期和受限 API 桥
src/                      React 会话工作台
scripts/                  开发、sidecar 和端到端验证
docs/                     产品计划和当前协议
```

完整设计见 [产品与架构计划](docs/architecture-plan.md)，本分支协议见 [持久事件与会话续接](docs/protocol-v6.md)，0.6 基线见 [主智能体与无头协议](docs/protocol-v5.md)，画布见 [第五阶段协议](docs/protocol-v4.md)，运行控制见 [第四阶段协议](docs/protocol-v3.md)，角色与执行器见 [第三阶段协议](docs/protocol-v2.md)，基础编排见 [第二阶段协议](docs/protocol-v1-draft.md)。开发进度见 [实施状态](docs/implementation-status.md)。

### 0.7 开发分支的任务控制

运行中明确选择补充当前任务或排队下一条；补充的接收、交付和实际结果分开展示。停止后可从指定历史任务创建新的继续任务，不重放旧任务。CLI 支持 `steer TASK_ID CONTENT`、`continue TASK_ID [CONTENT]`、`changes TASK_ID`。改动面板只展示有界执行区间差异，保留用户原文件和 index；为避免 Git filters 越权执行，概览只用有界 no-follow 解析器读取受支持的本地 SHA-1 index/HEAD/pack，不运行 Git。暂存元数据与原始工作区字节分列，转换和忽略规则未知；不支持的格式或安全失败明确显示未知。完整验证矩阵见 [0.7 验收跟踪](docs/acceptance-v0.7.md)。

### 0.8 开发分支的执行控制切片

设置、角色和单次任务可收紧预算与权限；Native 支持实际验收失败后的限次修复/复验、持久问题与逐次写入/命令授权。CLI 新增 `requests [TASK_ID]`、`answer REQUEST_ID REVISION TEXT|-`、`approve|deny REQUEST_ID REVISION DIGEST`，`run` 可用 `--controls FILE|-`。Codex 仅支持外层时长与公开输出限制，不支持的逐工具限制会拒绝，不能据此宣称已管理其内部 MCP。取消保留已发生副作用；在途文件写入会继续持有项目执行权直到清理完成，界面明确区分终态与清理状态。详见 [执行控制协议](docs/protocol-v7.md) 与 [0.8 迭代范围](docs/iteration-v0.8.md)。MCP / Skills、命名服务及系统凭据管理尚未由此切片交付。


### 0.8 托管能力开发切片

开发分支已实现并通过本地模拟门禁的应用管理 MCP 与固定版本 Skills，详见 [协议 v8](docs/protocol-v8.md)。MCP 注册/发现/启用不等于工具授权或实际执行；默认拒绝外部工具，Native 任务按固定目录和具体审批调用。MCP 是可信本机程序，不是文件/网络沙箱。Skills 只加载明确选择的正文，不自动运行旁边脚本。真实模型、外部服务、系统凭据及本地桌面 GUI 不在本轮模拟验证范围内。
