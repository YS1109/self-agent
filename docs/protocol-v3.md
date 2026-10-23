# 第四阶段协议

版本 0.4.0，在第三阶段角色与执行器协议上扩展运行控制、计划调整和命令验收。仍通过统一 `POST /api/v1/commands` 接收桌面或模拟远程指令，固定最初来源回调。

## 运行控制

以下指令均需要 `plan_id` 和 `expected_revision`：

```json
{"type":"pause_plan","plan_id":"plan-id","expected_revision":7}
{"type":"resume_plan","plan_id":"plan-id","expected_revision":9}
{"type":"patch_plan","plan_id":"plan-id","expected_revision":10,"nodes":[]}
```

`patch_plan.nodes` 必须提交完整非空依赖图；示例中的空数组只是字段示意，不是有效计划。

- `pause_plan` 支持 running、reviewing、waiting_input，状态转为 paused。停止新派发，正在实际执行的任务允许完成。等待项目写入锁的任务在启动执行器前检查暂停，恢复排队。
- 暂停会使未完成回顾作业失效，迟到响应不能推进计划。暂停前状态保存在 `paused_from_status`，恢复待确认计划不会绕过回顾轮次预算。
- `resume_plan` 仅用于 paused，按已保存节点状态继续。已完成任务不会重放。
- `patch_plan` 支持 running、paused、reviewing、waiting_input。仅 pending、queued 节点可以改描述、角色、依赖、关系或删除；已运行及终态节点定义必须保留。
- 修改排队节点会取消旧任务，保留旧任务及其回调，重新创建后续执行记录。旧任务的迟到结果不会污染替代节点。
- 调整回顾／待确认计划会开始下一轮；暂停中的修改仍保持暂停。累计最多三轮。
- 计划 `used_node_ids` 保留所有已创建节点 ID，累计最多十二个，删除不返还预算，已删除 ID 不能复用。草稿未启动前的编辑不消耗执行预算。
- 执行状态和证据变化会增加 revision。过期修改原子拒绝，前端保留本地草稿并要求重新加载；不自动覆盖后台历史。
- `cancel_plan` 仍负责立即取消计划、在途任务及回顾，不受暂停状态限制。

## 修复和验收

节点新增可选配置，旧节点默认没有关系或验收命令：

```json
{
  "id":"repair",
  "title":"修复测试失败",
  "objective":"依据先前失败修复实现，保留已有测试",
  "depends_on":[],
  "repairs":["failed-test"]
}
```

```json
{
  "id":"verify",
  "title":"重新验收",
  "objective":"实际运行 python3 test_calc.py 并报告结果",
  "depends_on":["repair"],
  "verifies":["failed-test"],
  "acceptance_command":"python3 test_calc.py"
}
```

`repairs` 和 `verifies` 只能引用已有 failed、blocked、cancelled 历史节点；不能引用自身、重复或不存在的节点。验收节点必须直接或间接依赖修复对应失败的节点。普通依赖仍需要实际完成，不把失败历史变成成功依赖。

任何配置 `acceptance_command` 的节点，后台将验收命令随目标明确传给执行器，只有实际执行器记录中匹配命令最后一次退出码为零，才允许任务成功。即使最终模型声称通过，没有证据仍判失败。命令比对去除首尾空白，保留其余完整语义；不能用其他成功命令代替。

验收节点完成、对应修复节点完成且依赖关系有效，后台才在原失败节点设置 `resolved_by`。原节点的 status、错误及证据保留，父计划只有在全部节点 completed 或失败已经有效验收解决时，才允许回顾 complete。被用户取消的整个计划不能通过修复恢复。

## 实际命令证据

任务和节点的 `evidence` 默认空数组：

```json
[{"command":"python3 test_calc.py","exit_code":0,"output":"ACCEPTANCE_OK\n"}]
```

证据仅从执行器工具事件产生，不接受模型最终文字或客户端修改证据：

- Native：来自真实 `run_command` 的退出码和 stdout/stderr；命令工具仍须显式 `HARNESS_ALLOW_COMMANDS=1`，不会自动扩大权限。
- Codex：仅接收当前 thread/turn 的 `item/completed`、commandExecution、终态和实际 exitCode。失败回合已产生的命令证据也会保留，重复 item ID 不重复记入。
- Codex 标准 `sh/bash/zsh -c/-lc` 包装只在严格解析为三个参数且无外层动态语法时解包；保留完整内部脚本，原包装命令写入输出首行。`test || true` 与 `test` 不等价。
- 每任务保留最近 64 条，单条输出最多 16 KiB，按 UTF-8 边界截断；超长命令不截断成可匹配前缀。
- 证据即时持久化，执行失败、取消或重启仍保留已提交记录。本应用已知 API Key 脱敏。

记录表明某个执行器实际执行命令及退出码。选择的验收命令是否充分覆盖目标，由用户和计划制定者决定；当前没有对后续文件变化建立内容快照，也没有提供防恶意执行器的证明。

## 验证

`scripts/runtime_smoke.py` 使用本地规划／执行模型夹具，驱动真实 Native 文件和命令工具，覆盖暂停、排队修改、版本冲突、失败、修复、验收、取消及固定来源回调，已加入三平台 CI。

`python3 scripts/runtime_smoke.py --real-codex` 使用本机 Codex 登录和模型，替换执行器部分；规划与回顾仍是确定性夹具。2026-10-23 在 macOS 隔离项目上完成真实 Codex 修改、测试失败、修复和复验闭环，保留原始测试文件，并验证暂停／恢复、中途补充、取消和回调。不据此声称已验证真实主模型的规划质量或其他平台的云端执行。
