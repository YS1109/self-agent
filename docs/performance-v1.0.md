# 1.0 长历史性能与资源实测

本页是开发分支的测量记录，不是 1.0 发布或性能 SLA。脚本为
[`scripts/history_benchmark.py`](../scripts/history_benchmark.py)，仅使用隔离的合成数据、真实后台/SQLite/HTTP/CLI；不调用模型、QQ、系统凭据库或外部网络。

先记录 `1e7d537b` 的原始失败基线，后附 **indexed scheduling v3 未提交候选**的同 seed 对照。后者改善空闲 CPU/回放，但尚未修复大快照/单任务 CLI 容量；不覆盖原始失败，也不将候选写成已发布版本。

## 测量对象与可复现性

- 来源候选：`1e7d537be41bb44b4b067984ee3a15db9f0127ff`（版本化存储、备份恢复与诊断切片）。并行开发中的 gateway/lifecycle 不属于本次测量。
- 使用父任务指定的 **debug** 二进制，在测试前复制到独立目录并核对源/副本 SHA-256；不重新编译共享 `target`。二进制未嵌入可独立验证的提交标识，因此来源提交与实际二进制摘要分别记录。
- 后台 SHA-256：`66e31a440fa16027b9cd4c8569b6a66fa0db5e060a62567318e325b9b3e5cccf`。
- CLI SHA-256：`b358dd07677863d44a0178a31dd6e553478a5f27efbc3f62c7adaf0de101bdfd`。
- 环境：Linux `6.18.55`，x86_64，Intel Xeon Platinum 8573C；可见/affinity CPU 均为 9；Python 3.12.14、Python SQLite 3.53.1。cgroup CPU/内存额度不可读，未推测其数值。共享云环境未隔离其他任务，也未执行缓存清空。

每份数据先由指定后台初始化真实 schema，再在后台停止时插入合成历史。所有任务均为已完成终态，无待执行计划、工具操作、外发回调或凭据操作。会话消息、任务、事件采用共享类型所需字段和显式旧版本默认兼容结构；另用真实备份及离线恢复路径验证业务记录可解码。恢复目标也是新建的隔离夹具目录，不是用户数据。

### 统计口径

- 100/1,000 任务集：HTTP、CLI、事件回放每项 10 次；启动 5 次；备份与离线备份检查各 3 次。1,000 任务已发生 10 次完整回放超时后，为限制额外资源消耗，1,500/5,000 任务集每项请求和启动各 3 次、备份检查各 2 次；**未改变内容、30 秒请求截止或客户端 16 MiB 上限**，也未混合两组样本。采用 **nearest-rank** P50/P95，不丢弃预热样本；小样本 P95 通常就是最大值，不能当作生产尾延迟保证。
- 启动计时为创建后台进程至首次成功 `/health`，轮询间隔 10 ms；重复启动使用温热文件缓存，**不是冷启动**。
- HTTP 计时至完整响应字节接收，不含随后的 Python JSON 完整性断言。CLI 包含进程创建、HTTP、JSON 输出以及父进程观察退出；`Popen.wait(timeout)` 的 POSIX 轮询会增加至多约 50 ms 的观察开销，尤其影响很短的 CLI 操作。
- SSE 从请求开始至消费指定数量的有序事件；逐条验证 `id == sequence`、连续性和数量。字节计数为已消费帧，不含 HTTP 头或未消费的预读字节。完整保留集最多 20,000 条；超出保留窗口的游标应返回 409，而不是偷偷补齐或截断。30 秒是本次测量的操作截止，不是新宣称的产品 SLA；超时说明未在该预算内完成，**不等于已证明事件丢失或乱序**。
- 备份计时为单次 POST 至 GET 观察到 completed，观察间隔 20 ms；之后单独测量 `backup-inspect`。每次实际备份的字节数、摘要和原始回执保留在原始结果中。备份副本检查后删除，避免为采样长期保留重复影像。
- RSS/HWM 仅读取 **后台进程**的 Linux `/proc/<pid>/status`。HWM 是该进程生命周期峰值，不是某个单独接口峰值；不包括 Python/CLI/系统页缓存，也不是进程树总量。其他 OS 的内存指标明确未测量。
- 空闲 CPU 为没有用户请求的 2 秒窗口内后台 `/proc/<pid>/stat` CPU 时间增量，100% 表示占用一个逻辑 CPU；内部轮询线程仍运行。窗口短且环境共享，只作为本次观察。
- HTTP 客户端测量上限 96 MiB、单数据库严格小于 128 MiB；应用已有快照客户端上限为 **16 MiB**。测量客户端能收完不等于应用客户端能使用；超过应用上限必须记为失败，不截断伪装通过。

## 结果

**已发现实际失败，不能据此宣称长历史门禁通过。** 1,000 任务集的 HTTP 快照尚未达到客户端字节上限，但 20,000 条事件完整回放 10/10 未在 30 秒截止内完成；1,500 和 5,000 任务集的完整快照超出 16 MiB，真实 CLI 拒绝，即使仅请求某一个任务也失败。所有四个规格均已完成下列测量，没有调高阈值来换取通过。

| 合成数据集 | 会话 / 完成任务 / 消息 | 输入 / 回复字节（每回合） | 保留事件 | SQLite bytes | HTTP 快照 bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| small | 10 / 100 / 200 | 512 / 1,024 | 2,000 | 2,293,760 | 633,093 |
| long_session | 1 / 1,000 / 2,000 | 1,024 / 4,096 | 20,000 | 29,528,064 | 15,389,909 |
| over_snapshot | 2 / 1,500 / 3,000 | 1,024 / 4,096 | 20,000（序号 10,001 起） | 38,453,248 | 23,021,086，**超过上限** |
| large_history | 100 / 5,000 / 10,000 | 512 / 2,048 | 20,000（序号 80,001 起） | 59,211,776 | 40,620,326，**超过上限** |

每个任务的 `result` 和 `stream_output` 各保留一份回复，会话内另保留用户/助手消息。任务使用合法的最小历史元数据，未追加真实工具证据、回调正文、计划或服务私有快照；这些真实数据可能让上限更早出现。因此 **1,000/1,500 不是通用容量承诺**。

### 已完成的 100 / 1,000 任务样本

单位 ms，`P50 / P95`：

| 操作 | 100 任务 | 1,000 任务 |
| --- | ---: | ---: |
| 温热重复启动 → health | 95.7 / 127.7 | 1,323.1 / 1,686.8 |
| HTTP 完整 snapshot | 41.8 / 64.5 | 1,553.0 / 2,198.0 |
| HTTP 固定诊断 | 6.5 / 9.3 | 1,102.0 / 5,409.6 |
| CLI status | 114.9 / 166.4 | 2,886.7 / 5,848.9 |
| CLI status 单任务 | 114.5 / 115.2 | 2,196.3 / 5,649.6 |
| CLI diagnostics | 16.3 / 66.5 | 1,071.5 / 1,950.6 |
| SSE 128 条回放 | 9.1 / 12.4 | 881.8 / 1,729.0 |
| SSE 全部保留事件 | 143.9 / 189.7（2,000 条） | **10/10 超时**（20,000 条；30 s 截止） |
| 在线备份完成 | 121.6 / 152.5 | 3,050.9 / 3,285.3 |
| CLI backup-inspect | 114.4 / 164.4 | 1,074.8 / 1,125.0 |

两集备份均实际创建、校验，并各用一份备份成功完成独立目标的业务类型恢复校验。1,000 集失败回放没有成功样本，因此 P50/P95 为空，不能只统计成功部分后写成通过；首轮脚本仅记录其超时原因而未保存失败经过时间，不能把配置的 30 s 伪装成精确逐次实测耗时。续测版本另外记录失败耗时及已消费事件数。

| 资源（后台进程，Linux） | 100 任务 | 1,000 任务 |
| --- | ---: | ---: |
| 2 秒空闲窗口 CPU（单核=100%） | 17.00% | **99.94%** |
| 空闲窗口结束 RSS bytes | 34,267,136 | 127,668,224 |
| 全部操作结束 HWM bytes | 57,159,680 | **577,544,192** |

HWM 覆盖该进程全部测量操作，不能归因于单个接口，也不能据此直接判定内存泄漏。

### 1,500 / 5,000 任务续测

单位 ms，`P50 / P95`；请求/启动 n=3，备份/inspect n=2：

| 操作 | 1,500 任务 | 5,000 任务 |
| --- | ---: | ---: |
| 温热重复启动 → health | 1,539.2 / 6,107.1 | 2,550.7 / 3,364.6 |
| HTTP 完整 snapshot | 4,022.0 / 6,272.6 | 5,990.6 / 8,342.3 |
| HTTP 固定诊断 | 4,934.2 / 5,928.0 | 3,950.7 / 4,688.5 |
| CLI status / 单任务 status | **各 3/3 拒绝，16 MiB 上限** | **各 3/3 拒绝，16 MiB 上限** |
| CLI diagnostics | 1,326.6 / 1,475.2 | 5,624.4 / 7,492.0 |
| SSE 128 条回放 | 1,537.7 / 2,220.2 | 7,157.3 / 7,664.5 |
| SSE 20,000 条回放 | **3/3 超时** | **3/3 超时** |
| 在线备份完成 | 2,923.0 / 3,501.3 | 8,060.1 / 9,744.5 |
| CLI backup-inspect | 1,218.3 / 1,520.1 | 1,847.0 / 3,947.1 |

1,500 集三次完整回放在约 30,000.7–30,001.5 ms 截止时分别只消费 **3,840 / 4,096 / 4,736** 条连续事件；5,000 集约 30,001.5–30,002.5 ms 时分别消费 **1,408 / 1,280 / 1,536** 条。没有将部分回放计为完整成功。两个已裁剪历史集的过期游标均实际返回 409。

| 资源（后台进程，Linux） | 1,500 任务 | 5,000 任务 |
| --- | ---: | ---: |
| 2 秒空闲窗口 CPU（单核=100%） | 98.96% | 99.98% |
| 空闲窗口结束 RSS bytes | 58,654,720 | 74,412,032 |
| 全部操作结束 HWM bytes | 445,431,808 | **1,101,455,360** |

共享环境、很短 CPU 窗口和分配器保留导致资源/延迟不必按任务数单调递增；不能把此表拟合成普适容量曲线。最大数据库仅约 56.5 MiB，已出现约 1.03 GiB 后台 HWM，直接说明 **128 MiB 备份影像上限不是 RSS 上限**。

### 通过 / 失败 / 未运行

- **通过**：快速正确性自测（percentile、失败人口统计、固定 ID/字节、完成态数据及非空库保护）；Python 编译；16 次启动；26 次 HTTP 完整快照（完整任务/消息数量断言，不截断）；26 次 HTTP 与 26 次 CLI 固定诊断；26 次 128 条有序回放；小集 10 次完整 2,000 事件回放；10 次真实备份及 10 次 CLI inspect；4 个规格各一次真实业务类型恢复校验；两个裁剪历史集过期游标 409。
- **失败**：1,000/1,500/5,000 集合计 **16 次完整 20,000 事件回放均未在 30 s 内结束**；1,500/5,000 集各自 `status` 与 `status <id>` 都 **3/3** 被真实 CLI 的 16 MiB 限制拒绝。脚本保留失败样本，续测进程最终退出码为 2；不是“所有门禁通过”。
- **未运行**：release 对照（避免与其他授权构建争用磁盘/编译资源）、采样 profiler/消融优化对照、冷缓存启动、真实 GUI/平台安装及其他 OS RSS。没有将这些未测阶段推断为成功。

### 原始证据与中断说明

本地证据根目录：`/workspace/scratch/ed5e723b0697/history-benchmark-1e7d537b/`。

| 原始文件 | SHA-256 |
| --- | --- |
| `measured-20261028/results.json`（100 / 1,000 任务，原值保留） | `631f03d63668d101fa116d34427ee559770efdbf8765738680c66e12fdfddf8b` |
| `continued-20261028/results.json`（1,500 / 5,000 任务） | `9575d5a80492eaa209479bdb01360e9efd33ee9768a8658c3bdaf00789a0fa60` |
| 首轮归档脚本 `measured-20261028/history_benchmark.py` | `2da34add65eb546eac9bc69939e7e4444d0aacf613448b34a459a94fb3eada51` |
| 续测归档脚本 `continued-20261028/history_benchmark.py` | `34e825eb67fd443ec6e703557d047b93fb522700d866a7f297efec06a56a3f7f` |

首轮在 100/1,000 规格完整结果落盘、下一规格尚未完成时遇到执行环境中断，故没有总运行结束标记。恢复后原 tool session 已不存在，三个旧夹具的 `daemon.lock` 均确认释放；**只使用已完整落盘的两组结果**，未把中断的 1,500 规格视为成功，也未重放其备份请求。原 JSON 未修改，另附 `interruption.json`。续测使用新私有目录，增加逐指标 `progress.json`、失败回放计数及 Unix 整组时间闹钟，不更改服务二进制、数据内容或失败判断。

各规格保留一份 `seed.sqlite`，其 SHA-256、准确记录数量、每次 HTTP/SSE/CLI 样本、备份 manifest/摘要均在 JSON 中。所有成功备份 bundle 和恢复校验目标检查后即删除；在确认后台锁释放后清理重复运行库和中断副本，共 **211,359,689 bytes**，记录在 `cleanup-20261029.json`。保留全部原始日志、结果、脚本和四份 seed；没有删除其他任务的文件。二进制副本仍在 `bin/`，供后续同源 before/after 对照。

### 只读源码定位（固定 `1e7d537b`，未进行优化）

- `crates/harness-daemon/src/lib.rs:798–821`：两个任务 worker 在空闲时每 300 ms 取得同一个 Store Mutex 并调用 `claim_task`。
- `store.rs:417–428`：每次领取先执行 `qq::reconcile`，再 `rows(tasks)`、排序、最后查找 queued。`rows`（1535–1540）先 SELECT 全部 body，再将每一条完整 JSON 解码；过滤条件没有下推 SQL。
- `store/qq.rs:454–484`：reconcile 自身两次扫描完整任务表，之后才检查是否活动任务及来源；即使本夹具未配置 QQ、全部任务已经完成，解析成本仍存在。因此每次空领取至少涉及三遍历史任务 JSON。`qq/daemon.rs:41–58` 的未配置 supervisor 仅每 500 ms 查询配置，不是本次外部 QQ 请求。
- `lib.rs:733–789` 的 SSE 每一批重新取得同一 Mutex；`store.rs` 的 `events_after` 每批 128 行。全量历史轮询与事件回放可能竞争同一锁；这与观测吻合，但**尚无采样 profiler 或消融对照，不能把全部延迟精确归因于这一处**。
- `store.rs:248–286` 的 snapshot 全量加载全部会话（含消息）及任务；CLI `main.rs:1130–1138` 即使 `status <id>` 也先取完整 snapshot；读取到 16 MiB 时 `main.rs:217–218` 明确拒绝。1,500 集的拒绝已由真实 CLI 重现，不是推算。
- `store/reliability.rs:689–711` 的诊断对任务 body 多次 `json_extract` 聚合；虽输出固定且小，仍会扫描历史。诊断脱敏正确不代表查询成本恒定。

后续应独立设计活动任务索引/候选查询、按会话/游标的有界历史分页和真实单任务读取，再以相同固定夹具复测；本任务没有修改共享 Store、调高响应上限、隐藏失败或把 release 未测结果补成通过。

## 第一阶段对照：indexed scheduling v3（未提交候选）

### 身份与迁移证据

- 来源为 `3160a4e2e0b0cf54030c56c43b50c6f8e136680c` **加未提交的 indexed scheduling v3**，不是上述提交本身或已发布版本。仍使用固定二进制，不构建共享 target。
- daemon SHA-256：`625e9f1045ea1f83308e2a085e18a41910c02ec724a576a39c84c77b1e1c3a3e`。
- CLI SHA-256：`2f34d47740774d812796ae11fe5f6d52a8f91a1bc6470ac9bea0ee44ff319f44`。
- 使用新增 `--seed-results`，从原 1,000 / 5,000 规格的 **同一 seed.sqlite 字节副本**启动迁移；没有重新生成更短消息、清掉历史或更改响应限制。源 seed 在运行前后再次校验 SHA-256，均未改变。
- 显式 schema 1→3 的单次首次启动分别为 **3,125.6 / 7,485.4 ms**；这是迁移样本 n=1，与下表后续启动分列。迁移后 SQLite 分别为 **29,708,288 / 59,641,856 bytes**。
- 两组迁移前后的 sessions/tasks/messages/commands/events 数量及会话、任务、收据、事件的逻辑内容 SHA-256 全部一致；没有活动任务产生。1,000 组逻辑 SHA 为 `4560e4f54669b6284e43b0c0bb4b449f71588fe31e7ceeb98c4b5ce5c522bff4`；5,000 组为 `ec2f9d3d711239e64640424e1c2ba152c494a32fb499f067fd1604e1a8ce9ea0`。

本次请求/启动每项 n=3，备份/inspect n=2；30 s 操作截止、20,000 条完整回放和 16 MiB 客户端限制均保持。候选还包含 `3160a4e2` 基线的其他变更，环境也非独占，因此这是实际候选 before/after，不是严格只有一个变量的因果实验。

### 实测结果（ms，P50 / P95）

| 操作 | v3：1,000 任务 | v3：5,000 任务 |
| --- | ---: | ---: |
| 温热重复启动 → health | 1,434.9 / 2,096.3 | 3,401.6 / 3,779.5 |
| HTTP 完整 snapshot | 758.8 / 940.2 | 2,181.8 / 2,982.8 |
| HTTP 固定诊断 | 23.4 / 26.0 | 71.0 / 86.3 |
| CLI status | 1,775.2 / 2,340.6 | **3/3 因 16 MiB 拒绝** |
| CLI status 单任务 | 969.0 / 1,318.3 | **3/3 因 16 MiB 拒绝** |
| CLI diagnostics | 64.3 / 64.6 | 114.5 / 114.5 |
| SSE 128 条回放 | 8.5 / 12.1 | 11.6 / 15.3 |
| SSE 全部 20,000 条回放 | **1,107.9 / 1,644.9，3/3 完整通过** | **1,335.8 / 1,689.4，3/3 完整通过** |
| 在线备份完成 | 998.4 / 1,083.5 | 1,916.8 / 2,899.2 |
| CLI backup-inspect | 1,167.3 / 1,167.7 | 2,021.8 / 2,203.7 |

完整 HTTP snapshot 字节数仍分别为 **15,389,909 / 40,620,326**，与旧 seed 测量完全相同；后者仍大于 16 MiB。不能用回放变快或诊断变小掩盖 CLI 容量失败；最终 runner 仍返回 exit 2。

| 资源/结果 | 1,000：原始 → v3 | 5,000：原始 → v3 |
| --- | ---: | ---: |
| 空闲 CPU（单核=100%） | 99.94% → **1.50%** | 99.98% → **1.50%** |
| 空闲窗口结束 RSS bytes | 127,668,224 → 44,736,512 | 74,412,032 → 31,031,296 |
| 全测量生命周期 HWM bytes | 577,544,192 → 474,009,600 | 1,101,455,360 → **778,694,656** |
| 完整 20k 回放 | 10/10 超时 → 3/3 通过 | 3/3 超时 → 3/3 通过 |

**通过**：两组迁移内容不变、6 次启动、HTTP/诊断/128 回放、6 次完整 20k 回放、4 次真实备份/inspect、2 次独立恢复业务校验；5,000 组过期游标仍为 409。**保留失败**：5,000 组两个 CLI status 路径各 3/3 拒绝。**未测/未完成**：release、profiler、跨 OS、分页后快照和真实单任务 API；HWM 仍高，重复启动也没有证明比基线更快，不能宣称全部性能问题已解决。

### 对照证据与复跑

目录：`/workspace/scratch/ed5e723b0697/history-benchmark-v3-indexed/measured-reused-seeds-final/`。

- `results.json` SHA-256：`9ea92c6f6d588f5b4f5875bc915d65e2d3ec6252899347b04e1e069290c28340`。
- 归档 `history_benchmark.py` SHA-256：`737b14053eb375b79f3fafe01c8ff65624eb1c9d87598fefe72b46563c4fd25c`。
- 首次试跑在全部 1,000 指标完成后遇到 runner 的末尾变量遮蔽错误；对应 `measured-reused-seeds/` checkpoint/log 和 `runner-failure.json` 保留，但**不纳入本节统计**。修正后在新目录完整重跑两组，没有重放旧备份 UUID。最终后台锁均确认释放。

```sh
python3 scripts/history_benchmark.py \
  --daemon /absolute/pinned-v3/harness-daemon \
  --cli /absolute/pinned-v3/self-agent-cli \
  --source-commit '3160a4e2 + uncommitted indexed scheduling v3' \
  --artifact-dir /absolute/new-private-v3-results \
  --seed-results /absolute/original/measured-20261028/results.json \
  --seed-results /absolute/original/continued-20261028/results.json \
  --profile long_session --profile large_history \
  --samples 3 --startup-samples 3 --backup-samples 2 --keep-data
```

## 运行与验证

```sh
python3 scripts/history_benchmark.py --self-test
python3 scripts/history_benchmark.py \
  --daemon /absolute/pinned/harness-daemon \
  --cli /absolute/pinned/self-agent-cli \
  --source-commit 1e7d537be41bb44b4b067984ee3a15db9f0127ff \
  --artifact-dir /absolute/new-private-results --keep-data
```

`--profile` 可重复指定 `small`、`long_session`、`over_snapshot`、`large_history`；样本数、请求截止与单数据集时间预算可显式调整。结果目录必须不存在；`--seed-results` 应只传入该工具已归档的合成 seed 结果，脚本核验 SHA、复制后再启动新后台。**SHA 是完整性检查，不认证数据来源；不要传入真实用户库或第三方数据库。** 默认删除自己创建的合成夹具，`--keep-data` 保留 seed SQLite 和最终运行目录。退出码 2 表示存在测量失败/客户端预算失败，不表示已成功优化。进程在正常、异常和终止路径中均清理；不会构建、更新共享可执行文件或自动优化数据。

## 不在本次证据范围

release/优化构建、真实模型等待与工具执行、真实多年用户数据分布、GUI 渲染/虚拟列表、Windows/macOS RSS、安装升级、网络延迟及公网网关。任何容量结论只适用于记录的消息/任务体积和本次候选，不能把任务数量直接换算成普适用户上限。

## 第二阶段对照：reviewed v14 有界历史候选（未提交）

### 固定身份与同 seed 验证

正式结果时间为 **2026-10-29 01:26:10–01:28:38 UTC**；来源是 `6f497bae825d71fa2ec5a1c4b5c4a659258c9e1b` **加未提交的 reviewed v14**（含显式 window、current_cursor、权威错误码与 CLI 等待超时最后观察值修复），不是该 base 提交本身或已发布版本。

- 固定 daemon SHA-256：`31a68022aa5dc01b0babe849bbedbe6abcd77799b22addcc51d6fc53f2099c16`。
- 固定 CLI SHA-256：`90baca16f344eadb004978b57f4de97401b5f24678cd8a9fe4d75a5c7cfa3e1f`。
- 使用 `history-final-frozen/target/debug/` 的独立固定文件，没有构建/使用正在变动的主 target。环境仍为上述 Linux debug/shared cloud，未做 release、其他 OS 或冷缓存对照。
- 原 1e7d 的 1,000 / 5,000 两份 seed 字节副本迁移至 schema 3，源 seed SHA 与前节一致；迁移前后、全部只读测量/备份/重启后的 sessions/tasks/messages/commands/events 数量及逻辑内容 SHA 全部一致。两组源 seed 最后再次验证未改变。
- schema 1→3 的首次启动（各 n=1）分别为 **5,255.7 / 12,476.3 ms**；迁移后数据库仍为 **29,708,288 / 59,641,856 bytes**，不是更短的数据集。

### 口径：窗口不冒充全量，分页另作完整性验证

新增 `--bounded-history` 才请求 `/api/v1/snapshot?window=1`。逐次验证 `history.version=1`、epoch UUID、真实 total/returned/omitted/has_more 和消息 offset，并在 JSON 解析前限制实际响应字节：**window 12 MiB、page 2 MiB、task detail 8 MiB**。原无 query `/snapshot` 单列观察：本次两组各 3/3 明确返回 413 `record_too_large`，未把部分窗口谎称为完整数据。这是预期兼容拒绝，不计成新客户端容量失败；原先的 16 MiB baseline 失败仍保留。

旧模式不启用该 flag，继续要求完整快照数量；没有降低旧标准或覆写前两阶段原始结果。本阶段默认页面确实只含一部分记录，不能拿窗口缩小后的字节/耗时宣称“同样完整内容传输更快”。

| 实际响应/窗口内容 | 1,000 任务 | 5,000 任务 |
| --- | ---: | ---: |
| snapshot wire bytes | 913,091 | 2,577,762 |
| 窗口任务 returned / total | 80 / 1,000 | 80 / 5,000 |
| 窗口会话 returned / total | 1 / 1 | 59 / 100 |
| 窗口事件 returned / retained | 80 / 20,000 | 80 / 20,000 |
| 选取的窗口外 task detail bytes | 18,883 | 5,807 |
| tasks 页最大 wire bytes | 811,948 | 443,301 |
| messages 页最大 wire bytes | 220,471 | 117,899 |

5,000 组会话窗口还列出 21 条 `unavailable: record_too_large`（集合分配预算不足）；这**不是**证明这些单条历史真的超过分页上限，剩余遗漏通过 total/omitted 显示。所有 100 个会话的消息随后都成功完整分页读取。页面中的小型合成记录若 unavailable 会直接让本 runner 失败，不以它替代内容验证。

### 实测延迟（ms，P50 / P95）

除单列说明外，请求/启动各 n=3、备份/inspect 各 n=2。每个 HTTP/SSE/CLI 请求的预算仍为 30 秒；全部成功样本与失败人口分开保留，未丢弃较慢样本。

| 操作 | v14：1,000 任务 | v14：5,000 任务 |
| --- | ---: | ---: |
| 温热重复启动 → health | 1,218.0 / 1,657.0 | 6,836.4 / 7,153.3 |
| HTTP 有界窗口（不是完整历史） | 517.1 / 523.0 | 1,467.8 / 1,637.6 |
| HTTP 固定诊断 | 26.8 / 31.4 | 428.3 / 593.3 |
| HTTP 单任务 detail（窗口外） | 111.1 / 226.3 | 149.4 / 149.9 |
| HTTP tasks 第一页 | 213.9 / 243.5 | 103.7 / 135.1 |
| CLI status 有界窗口 | 466.3 / 469.0 | 868.8 / 871.0 |
| CLI status 单任务（窗口外） | 164.6 / 168.3 | 115.3 / 165.9 |
| CLI 完成态 follow（窗口外） | 214.9 / 216.0 | 168.3 / 222.1 |
| CLI history tasks 单页 | 316.3 / 365.6 | 220.8 / 268.7 |
| CLI diagnostics | 64.4 / 65.3 | 116.9 / 117.3 |
| SSE 128 条回放 | 18.3 / 19.3 | 10.2 / 27.8 |
| SSE 完整 20,000 条回放 | 1,044.7 / 1,344.1 | 1,351.4 / 1,568.8 |
| 在线备份完成（n=2） | 1,374.8 / 1,878.5 | 1,936.9 / 3,279.1 |
| CLI backup-inspect（n=2） | 1,173.8 / 1,782.8 | 3,275.6 / 3,882.3 |
| 旧无 query snapshot：预期 413 | 10.4 / 14.8 | 19.7 / 28.8 |

### 全历史分页验证（每类完整遍历 n=1，不是单请求 P95）

任务和消息分别核对完整 ID 集、跨页去重、稳定 anchor/total、消息 offset 连续，以及原 seed **每个字段值精确相等**（包括完整 result、stream_output、content；共享类型新增默认字段允许存在）。窗口/detail 内已返回的事件也核对原字段，只有 `RunEvent.data:null` 按已声明的 serde Option 语义允许省略，非 null payload 仍严格比较；脚本自测会拒绝截断。**完整 20k SSE 的独立门禁是连续 sequence/id 与数量验证，不是全部 payload 的逐字段 oracle。**任务选择从实际窗口之外的原 seed ID 集获得，不假定某个 ID 恰好位于窗口内。

| 遍历 | 1,000 任务组 | 5,000 任务组 |
| --- | ---: | ---: |
| 所有 tasks | 1,000 条 / 13 页 / 2,491.9 ms | 5,000 条 / 63 页 / 7,246.1 ms |
| 所有 messages | 2,000 条 / 25 页 / 3,149.6 ms | 10,000 条 / 200 页 / 4,710.0 ms |

完整遍历总耗时包含 Python 的原 seed 校验与集合检查；逐页 HTTP 时间另记于 `page_requests`，不可将两者混合。每一页仍有 30 秒请求预算，整个多请求遍历另受每组 600 秒截止约束。Python 校验器持有 ID 集和至多一个会话的消息，**这不是 GUI、后台或进程树内存有界的证据**。只读 CLI history 各 3 次均仅返回一页，保留 total/has_more；没有自动积累所有历史。

### 资源与仍未解决的启动边界

| Linux daemon 资源 | 1,000 任务 | 5,000 任务 |
| --- | ---: | ---: |
| 空闲 CPU（单核=100%） | 2.00% | 3.49% |
| 空闲窗口结束 RSS bytes | 46,026,752 | 32,432,128 |
| 主测量进程生命周期 HWM bytes | 181,362,688 | 241,729,536 |

HWM 比 v3 对照的 474,009,600 / 778,694,656 bytes 低，但本阶段操作集合不同、环境共享，不能当作严格单变量内存优化实验或固定内存上界；备份 CLI/恢复进程与 GUI 内存未计入。

**启动仍有风险，不能称全部性能问题解决。** 5,000 组温热启动原始样本为 **7,153.3 / 6,836.4 / 5,701.6 ms**，其中 2/3 超过本候选 Native bridge 的 6 秒 `ensure_daemon` 等待值。这里由源代码截止值与 headless 实测作出桌面启动体验风险推断，**不是已运行 GUI 并复现其失败**；本脚本的 30 秒启动门禁全部通过。共享环境波动明显，也没有证明启动比 v3 更快；5,000 组 HTTP diagnostics P95 从 v3 的 86.3 ms 变为本轮 593.3 ms，同样保留较慢观察，不宣称所有指标更快或由本改动单因导致。全部记录为 completed-only，不能由低空闲 CPU/回放改善推断活动主任务领取成本恒定。

### 通过 / 失败 / 未运行

- **通过**：最终两组均 exit 0；6 次温热启动；6 次窗口、6 次单任务 HTTP、6 次任务第一页 HTTP；全部 6,000 个任务与 12,000 条消息的四次完整分页遍历；CLI status、窗口外 status/follow、history 单页、诊断各 6 次；6 次 128 回放、6 次完整 20k 回放；4 次备份/inspect、2 次真实业务类型恢复；两组重启后任务/消息旧游标共 4 次 409，新 epoch 改变；5,000 组被裁剪 SSE 游标仍 409；源 seed 和不可变历史摘要均不变。
- **保留失败/限制**：原 baseline/v3 全部失败样本不变；旧 no-query snapshot 的 6 次 413 明确列出；桌面 6 秒启动等待风险未消除。首次 runner 自身错误及旧 CLI 候选数据保留但不混入本最终表。
- **未运行**：本轮 GUI/WebView 测量、release、采样 profiler/严格消融、其他 OS RSS/性能、活动任务压力与真实模型；不把 parent 的独立联合测试当作本性能脚本测过的项目。

### 原始证据、重跑与清理

最终目录：`/workspace/scratch/ed5e723b0697/history-benchmark-v14-bounded/measured-final-cli/`。

- `results.json` SHA-256：`d98a208808888a94119cc4093ca12a7bc23223acfe48021ff27ad54048afc79c`。
- 归档/current `history_benchmark.py` SHA-256：`c272b76c045f809a8de85c10606823507fe31c1575ece31be36718d744d42dee`。
- `final-binary-source.json`、`final-cli-run.log` 位于其父目录；每组 `progress.json`/daemon.log 和原始逐项样本保留，两组 daemon.lock 已验证释放。
- `measured-reused-seeds/` 首次尝试因 runner 对终态 null 字段省略过严，以及 urllib HTTPError socket wrapper 层级错误而失败；已补确定性自测，未降低字节/时间上限。原 results SHA 为 `1598385e39214dd6c6ad2438dc64a70aaf882f9bcc36f4e02abccd9acf0f4322`，附 `runner-failure.json`，不作为产品失败或最终统计。
- `measured-reused-seeds-final/` 是旧 CLI `88cdca0a…` 的完整通过候选，结果 SHA 为 `3ff8d4e4cf30397b759610095c82cb7d00fd968492f4e9958b5ce968284ac068`。其后独立 headless smoke 发现 --wait 超时输出问题，故使用新独立 CLI `90baca16…` 完整重测；旧样本加 `superseded-candidate.json`，没有混 hash。成功备份/restore 影像验证后立即删除；两轮已结束的重复 DB/seed 副本在核验锁释放与历史 hash 后清理，仅保留本轮数据及全部原始日志/结果；原四份 1e7d seed 不动。

```sh
python3 scripts/history_benchmark.py --bounded-history \
  --daemon /absolute/history-final-frozen/target/debug/harness-daemon \
  --cli /absolute/history-final-frozen/target/debug/self-agent-cli \
  --source-commit '6f497bae + uncommitted reviewed v14 + CLI wait fix' \
  --artifact-dir /absolute/new-private-bounded-results \
  --seed-results /absolute/original/measured-20261028/results.json \
  --seed-results /absolute/original/continued-20261028/results.json \
  --profile long_session --profile large_history \
  --samples 3 --startup-samples 3 --backup-samples 2 \
  --timeout 30 --case-timeout 600 --keep-data
```

脚本 self-test、Python 编译与空白检查均通过。所有 SQLite 临时连接显式关闭并保留事务提交，避免 Windows self-test 清理时持有文件句柄；此处自测通过不等于已测 Windows 性能。
