# 协议 v14：有界工作区快照与所有者历史读取（开发分支）

本文描述当前已接线切片；最终门禁与精确提交 CI 见 acceptance-v1.0，不代表生产发布。运行包版本仍为 0.6.0；schema v3 调度见 [v13](protocol-v13.md)。所有新接口只接受本机 owner bearer，不向 QQ/H5 开放管理代理，也不赋予查看历史者额外执行权限。

## 1. 快照是窗口，不是全部历史

显式选择 `GET /api/v1/snapshot?window=1` 保留公共集合字段，但最多 12 MiB。后台直接从 SQLite 选择有界记录/消息窗，不先构造完整快照再裁剪。原任务结果、证据、stream_output 不缩短；超过窗口预算的整个记录有明确遗漏信息，而不是伪造空结果。历史记录不删除。

新增 `history={version:1,anchor,collections,session_messages}`。anchor 是本次 daemon 实例的规范 UUID。每个集合包含 `total,returned,has_more,omitted,pending,pinned,pinned_pending,unavailable`：returned 是实际数组长度，pending 为未装入窗口的活动记录数，不是系统所有待执行工作的总数。界面必须区分局部窗口和总数、重要项未载入与真正不存在。 `unavailable.reason=record_too_large` 是相对于本次响应或集合预算的不可载入，不是永久不可读标志；窗口中被标记的记录仍可通过分页或单条接口读取，不能据此跳过其它只读入口。

本机持久会话选择及对应会话头优先，活动任务/待处理决策优先于旧结果；前端本地选中的历史任务通过独立 detail 保留，不能因它不在窗口就切换成另一个任务。会话消息窗口包含 `offset,total,returned,has_more_before,unavailable`；消息有绝对 `history_offset`，超大项缺口不能靠数组长度推算。未加载较早消息不表示没有历史。

公共服务/能力元数据仍是白名单投影；其合法体积超出快照预算时返回明确 `history_capacity`，不伪造空注册表。没有读取 vault 秘密或改变权限。

旧无 query `/api/v1/snapshot` **不返回窗口**：保持原完整集合语义，超过保守历史预算或 16 MiB 编码上限则静态 413，提示新客户端使用窗口/分页。不会让不识别 metadata 的旧客户端把遗漏任务判不存在；新浏览器/CLI/Native均显式 opt-in。拒绝其它或重复 window 参数。

## 2. 只读分页

- `GET /api/v1/history/{sessions|tasks|plans|plan_changes|deliveries|execution_requests}`，仅可选 `session_id` 与 `cursor`。
- `GET /api/v1/history/sessions/{id}/messages`，仅可选 `cursor`。
- 每页最多 80 个 items + unavailable，最终 JSON 编码最多 2 MiB。
- 响应为 `{items,total,returned,has_more,current_cursor,next_cursor,anchor,event_cursor,unavailable}`；消息另有 offset，items 按绝对消息索引升序展示。sessions 页只含会话头和 `messages:[]`，必须结合 `session_messages` 说明消息未加载。

每页都返回自己的 current_cursor，首次页也固定上界；返回上一页或刷新当前页必须复用它，只有显式重新读取首页才不传 cursor。游标不透明且绑定实例 epoch、集合、会话作用域与首次上界。记录采用固定 rowid 上界/keyset，消息采用固定数组索引上界；翻页期间新追加记录不会插到旧页造成跳项。页面之间不是数据库历史版本快照：既有任务状态可更新，每页都提供读取时 event_cursor。重启/恢复后 epoch 不同，旧游标返回 409 `history_reset`；客户端清除旧位置并只读获取首页，不重发命令。

路径/查询标识只接受 `[A-Za-z0-9_-]{1,128}`，游标最多 2048 个 ASCII 字符。拒绝重复或未知查询参数、百分号编码路径变体和不安全分隔符；旧记录含非路由标识时只能展示，不拼接成可执行路径。超大记录明确列为 `unavailable` 并推进游标，不无声丢失或返回空页无限循环。无法安全表示的标识/元数据返回静态错误。

## 3. 单任务与错误

`GET /api/v1/tasks/{id}` 返回 `{task,event_cursor,events,execution_requests,history}`，最多 8 MiB；task 保留完整公共权威内容。附带事件/请求是有计数的窗口，不代表全部审计；请求元数据另含未载入 pending 数，数组为空也必须显示遗漏。task、附属窗口与 cursor 在同一 Store 锁下读取。

- 404 `not_found`：确实不存在。
- 413 `record_too_large`：完整记录超过接口限制，未截断。
- 409 `history_reset`：旧游标不可用。
- 400 `invalid_history_query`：不支持的查询。
- 429 `history_busy`：两个有界读取槽已占用，可稍后只读重试。
- 503 `history_capacity` / 500 `history_record_invalid` 或 `history_unavailable`：明确不可用，不输出底层数据库错误或私有原始 body。

SQLite 读取移至 blocking worker；HTTP 断连不会无限堆积任务，读取槽由实际工作持有直到结束。客户端在 JSON 解析前执行字节上限，错误不回显服务器原始正文。仅匹配 HTTP 状态与最多 4 KiB 错误 JSON 的精确 code 才判定 not_found/reset/too_large；旧后台未知路由的空 404 或 HTML/无 code 响应是 unavailable，不冒充任务不存在。

## 4. 客户端与活动调度

CLI 单任务 status、follow 初始/恢复/终态查询与等待使用单任务接口，不再获取完整历史。分页命令只读单页，不自动积累无界结果。sessions/requests 在集合或消息有遗漏时返回含原数组、history、event_cursor 和提示的 envelope；完全载入时保留既有数组格式，不把空的局部请求窗口冒充没有请求。旧 UUID、未知命令结果、清理 notice 和不自动重发原则不变。

桌面/Web 历史页与 live reducer 分离，只有有界当前页及有限返回位置栈；关闭、切换范围和取消使旧响应失效。继续/取消只调用原本所有者命令入口，历史读取不提供新的批准权威。live 缓存超预算须恢复有界权威快照，不把丢弃增量伪装为成功消费。

主任务同会话前序检查也改为 SQL 仅筛活动状态后比较精确 u64 序号，保留大整数顺序及原串行语义；不再解码所有已完成任务。

## 5. 验证口径

需覆盖真实 HTTP/CLI/原生桥、前端解析器与后端响应联合契约、5000 历史同 seed 的完整分页/单任务/SSE/备份恢复及资源对照。编译/单测不等于真实 GUI，已知完整快照容量失败只有在实际修复复测后才可关闭。当前真实模型、OS vault、QQ、公网部署和安装发布均未运行。
