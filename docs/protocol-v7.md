# 第八阶段协议：任务执行控制（开发切片）

日期：2026-10-28。状态：开发中；本地模拟验收进行中，尚未发布完整 0.8。沿用 v6 的本机认证、事件游标、补充、继续与只读改动接口。

## 固化控制

公开设置新增 `execution_controls`；`run_agent` / `start_task` 可附带部分 `controls` 对象。角色可设置 `controls`。任务入队保存有效 `controls`；设置、角色和本次要求取限制交集，不提高上限、不删除既有验收要求。修改设置不改历史快照，但新撤销的权限会在下次效果执行前阻止旧任务。设置的部分补丁保留省略字段，旧客户端省略角色 `controls` 时保留原限制；只有明确 `null` 才清除角色附加限制（仍受全局上限约束）。

`continue_task` 创建新的计量记录，但继续保留原任务限制和验收要求，并与当前设置取交集；不会因为点击继续而静默恢复写入权限。需要放宽限制时，应明确配置并新建目标，而不是重放旧动作。Codex 运行时发现应用权限被收紧会停止，因为不能假装已经把新限制施加给其内部工具。

```json
{
  "budget": {
    "max_model_requests": 32,
    "max_runtime_seconds": 900,
    "max_retries": 0,
    "max_repair_attempts": 2,
    "max_output_bytes": 262144
  },
  "permissions": {
    "read_files": true,
    "write_files": "allow",
    "run_commands": "allow"
  },
  "acceptance_commands": []
}
```

范围：模型请求 1–128、运行 1–900 秒、安全重试 0–3、修复 0–8、公开输出 1024–262144 字节。最多八条验收命令，每条非空、不重复且不超过 16000 字节。`allow` / `ask` / `deny` 表示已有能力内允许、逐次询问或拒绝，不创建系统权限。

`HARNESS_ALLOW_COMMANDS=1` 仍是 Native shell 的必要开关。该 shell 是可信本机执行，不是项目文件系统或网络沙箱。禁止文件读/写时必须同时禁止 shell；文件写入需要授权时 shell 也必须授权或禁用。模型和角色文字不能修改这些限制。

Codex 仅支持本切片的运行时长和公开输出字节限制；其内部模型次数、修复次数和重试次数不由本应用计量，默认数值仅为兼容字段。明确提交这些内部预算、验收闭环或收紧逐工具权限会在启动执行器前被拒绝。不能把命令沙箱解释成已限制本机 Codex 自行加载的 MCP。

## 计量、停止与复验

任务新增 `budget_usage`：`model_requests`、`retries`、`repair_attempts`、`output_bytes`、`mutation_generation`、`started_at`。它们由 SQLite 保存，不从可淘汰事件推算。模型请求包括实际压缩请求；每次安全重试再次计模型请求。只重试尚未读取正文的明确 HTTP 429/503 拒绝，不重试响应读取错误、协议解析错误、未知发送结果或工具写入。

运行计时在任务取得项目执行权并准备实际执行时开始，包含模型、工具和等待用户的时间，不包括排队时间。问题/授权还有独立等待期限。取消和预算耗尽均保留已有改动、证据及明确 `stop_reason`；终态继续使用 `completed` / `failed` / `cancelled`，只生成一次原来源回调。停止原因遵循先到先保留；主动取消为 `user_cancelled`，后台中断执行为 `daemon_interrupted`，中断等待为 `interrupted_wait`。

输出预算计公开回复与工具结果的 UTF-8 字节，不伪装为精确模型 token 或计费额度。同一逻辑工具结果在事件、命令证据和复验记录中的重复表示按最长公开表示预留一次额度，先预留后持久化；额度不足时不保存该结果文本，但保留已观察到的退出码等元数据并停止。它不计用户输入、控制元数据或独立有界的改动报告，也不代表磁盘总占用上限。上游用量字段继续区分实际报告与估算。原有单帧、单次响应、文件和事件总数安全上限继续生效。

Native 在模型给出最终回复后，实际运行固化的验收命令批次。失败证据保存到追加式 `verification_attempts`，将真实失败结果反馈给模型修复，扣除修复预算，再执行原命令批次。最终文字不能替代退出码。模型控制的文件写入或 shell 操作会推进修改代次，旧批次成功不再有效。验收批次共享一个代次；这描述执行顺序，**不保证任意验收脚本只读，也不证明外部编辑器没有修改文件**。既有计划节点验收关系保持兼容。

### 文件与清理边界

Native 文件能力从保存的项目根目录逐组件 no-follow 打开，执行期间复用固定句柄；不先 canonicalize 跟随被替换的符号链接。相对文件路径拒绝越界、链接和特殊文件；写入用同目录临时文件、同步后原子替换，避免截断项目外共享硬链接 inode。不承诺保留 ACL/xattr。`read_files=false` 时跳过文件改动基线/报告；运行中撤销读取后不再公开本次报告。

`execution_cleanup`（可选 `pending` / `settled` / `unknown`）和 `execution_cleanup_updated_at` 描述已开始的阻塞文件写入。pending 在调度写入前持久化；取消或时长耗尽立即阻止新工具，但不能撤销已经进入内核的写入。执行器等待该写入完成并保持项目单写租约，此前不生成稳定结束报告。旧接口可以立即显示 cancelled；它不等于副作用已停止，CLI follow 仍按原终态退出并发出清理提示。settled 不等于回滚或任务成功；进程重启将 pending 标记 unknown，不声称已完成，也不自动重放。时长预算不保证不可中断的内核 I/O 或清理能立即结束。

可信 shell 与 Codex 的普通子进程组在停止时尽力终止并等待；不声称可约束恶意脱离进程组或系统级不可中断工作。Codex 配置/版本预检也接受运行时取消，在启动回合前预算耗尽时不继续开启 thread/turn。

## 待回答问题与逐次授权

Native 新工具 `request_user_input` 接受 `prompt` 和可选 `timeout_seconds`（默认 300，范围 1–900）。问题不含授权动作；工具返回用户原回答。需要逐次授权的写入/命令先产生独立请求，未经批准不执行。

问题及回答还保留在任务的 `question_answers` 中，后续会话历史能使用此前澄清；历史批准不会被拼接成未来任务的授权。

快照新增 `execution_requests`。记录包含 `id`、`task_id`、`session_id`、`kind`（`question` / `approval`）、`prompt`、安全动作描述 `action`、`action_digest`、`revision`、`status`、`answer`、`decision_actor`、`expires_at_ms`（Unix 毫秒）和审计时间。动作包含实际参数摘要；写文件不将完整内容写入审批记录。来源路由字符串不作为决定者身份，当前认证入口固定为本机所有者。

```json
{"type":"answer_request","request_id":"...","expected_revision":0,"answer":"保留原文件"}
```

```json
{"type":"decide_request","request_id":"...","expected_revision":0,"approve":true,"action_digest":"快照中的精确摘要"}
```

两类指令复用原幂等信封。回答不能替代批准；类型、修订、摘要或任务不符均拒绝。同一 `command_id` 重传返回同一收据，不重复执行。批准仅对应本任务本次具体动作，不成为会话或未来任务的通行证。

请求状态：`pending`、`answered`、`approved`、`denied`、`expired`、`cancelled`。任务等待时为非终态 `waiting_input`。HTTP 只保存决定；原存活执行器消费决定后才恢复运行。批准在消费时再次检查期限与撤权。拒绝或超时停止依赖动作，不能根据沉默推断同意。

等待不持有 SQLite/设置锁，管理查询、取消和其他项目仍可用；保留当前项目单写租约，避免用户审批期间其他任务改变该动作所依据的工作区。后台重启使原等待执行器失效：任务失败、未处理请求取消、历史决定保留，不自动重放。可明确发起新的继续任务。

事件新增 `task_control_updated`、`request_pending` / `request_answered` / `request_approved` / `request_denied` / `request_expired` / `request_cancelled` / `request_consumed`。这些是持久通知，客户端应刷新权威快照，不从通知文案推算完整请求或预算。

## 实施与验证界限

默认验收使用隔离项目、loopback 模型和 stdio 协议夹具。新增 `budget_smoke.py` 与 `execution_control_smoke.py` 覆盖实际文件工具、真实本地命令、失败/修复/复验、预算、授权、取消、过期和重启；具体通过项以同提交的验收记录为准。MCP / Skills 管理与命名服务、系统凭据生命周期仍属于后续 0.8 切片，不能由本协议宣称完成。
