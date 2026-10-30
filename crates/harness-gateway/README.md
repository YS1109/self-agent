# 独立自托管网关（0.10 开发切片）

仅设备认证目录 SQLite 持久化。任务/配对授权/命令回执仍在 daemon；离线不排队，未知结果不重试。浏览器会话在内存，重启须重新配对。此 crate 是 workspace 中独立可执行程序，使用 harness-core 的唯一 wire 定义；当前仍在主程序联合验收，不代表已通过真实网络验证。

```sh
cargo build -p harness-gateway
pnpm build:mobile
harness-gateway --db ./devices.db enroll '我的电脑'
harness-gateway --db ./devices.db list
harness-gateway --db ./devices.db revoke DEVICE_UUID
harness-gateway --db ./devices.db serve --origin https://agent.example.invalid --bind 127.0.0.1:8788
```

`enroll` 仅一次 stdout 输出 transport_token；请在可信终端操作，勿粘贴浏览器/日志。SQLite 只存 SHA256 verifier。数据库及父目录应仅服务账户可写；Unix 数据库权限 0600。没有远程注册、凭据读取或 owner API 代理。撤销为持久操作，在线设备连接每秒重新检查；每个请求另查实时目录。

nginx 配置见 `deploy/nginx-gateway.conf.example`。应用不信任 Host/Forwarded 来决定 Origin。必须显式固定 HTTPS Origin，绑定仅 loopback。测试须显式 `--fixture --origin http://127.0.0.1:PORT`，拒绝 localhost、非 loopback 或自动 HTTP 降级；夹具 cookie 不可当生产配置。

## 浏览器接口

响应均 `Cache-Control: no-store`，JSON 为 `{code,data?}`。所有请求须 `Sec-Fetch-Site: same-origin`；写请求还须精确配置的 `Origin`、`x-csrf-token` 和 JSON content-type。Cookie 由浏览器自动附带。不得将设备 transport token 放入浏览器。

- `GET /gateway/api/session`：建立/恢复匿名 cookie，`data={mode,csrf,paired,device_id,principal_id,binding_revision,expires_at_ms,permissions,session_ids}`。失效 cookie 换全新匿名会话，不恢复旧 principal。
- `POST /gateway/api/pair {device_id,code,label}`：分配一次 principal；同一 cookie 不可再次 PairClaim，包括未知结果。后续只读 `GET /gateway/api/pair-status`。退出后新会话可以用新码配对。
- `POST /gateway/api/rpc`：直接传 wire `GatewayRpc`，禁止 PairClaim/PairStatus；principal/revision 仅来自 cookie。active PairStatus 必须给精确 principal、binding_revision、expires_at_ms、permissions、session_ids，才获得会话权限。daemon 每次仍校验授权。
- `GET /gateway/api/events?session_id=UUID&after=N`：SSE `event: gateway`，data 为 GatewayResult，`id` 为 daemon `data.cursor`。非 Ok 发送后结束；断线不得把最后收到的游标改造成猜测值。
- `GET /gateway/api/devices`：仅返回当前 active binding 的设备元数据，不公开目录。
- `POST /gateway/api/logout`：销毁 cookie 会话；原授权审计仍在 daemon，但新 cookie 无法冒用该 principal。

设备 `GET /gateway/device/UUID` WebSocket 使用独立 `Authorization: Bearer TOKEN`，拒绝 Origin（设备不是浏览器 WS）。5 秒内第一帧必须是 wire1 Ready，重复活跃连接拒绝；15 秒 Ping、45 秒无响应关闭。JSON 帧/消息<=256 KiB，未知字段/重复或无请求 Reply 关闭连接。已经发送但超时、观察中止或 principal 撤销的请求，保留至多 256 个本代连接标识；唯一匹配的迟到回复只丢弃、不恢复会话或再次提交，达到上限则关闭连接而非遗忘未知事实。RPC20秒超时，可能发送后返回 outcome_unknown；只能读取原 command_id 回执，绝不自动重发。

上限：目录1024设备、内存1024浏览器会话（其中未授权最多256；匿名5分钟、待确认10分钟、首次授权后固定12小时，不因轮询延长）、设备连接1024、每设备8/全局64 in-flight，设备发送队列16；SSE全局64/每会话2、每流16个响应队列，5秒发送积压终止生产，不继续请求 daemon。生产 nginx 额外10秒慢客户端超时，并对 bootstrap/pair 路由按来源 IP 限速6次/分钟、burst6；这些是基本容量约束而非分布式抗DDoS服务。目录验证最多32个并发；暂时繁忙/存储不可读时请求失败关闭、设备连接可断开，但不销毁浏览器身份。只有权威目录确认删除、撤销或修订失效才清除相关 cookie 会话。无自动任务或离线队列。

验证限本机模拟 HTTP/WS、SQLite，并已接入真实 daemon + gateway + 本地模型协议夹具；这不意味着真实域名/TLS/公网/移动网络或 OS 凭据已验证。最终门禁见 `docs/acceptance-v0.10.md`。

## 本机配置与首轮授权

构建 `dist-mobile/` 后，由用户将其放到 nginx 的静态目录，网关进程单独运行。以上示例不会自动创建公网服务或证书。

1. 在可信服务器终端执行 `enroll`，保存设备 UUID 与一次性输出的独立 token；不要把 token 放进 URL、shell 参数、H5 或共享日志。
2. 桌面「自托管网关 / H5」停用创建固定 HTTPS origin + 设备 UUID。独立设备 token 仅经本机系统凭据入口写入，或 `self-agent-cli gateway credential-set CONFIG REV --stdin` 的标准输入；无 OS 存储成功就不降级到明文配置。
3. 明确启用新配置修订；就绪只证明认证设备连接，真实公网/TLS 行为需用户单独验证。修改来源/设备前必须解绑凭据，配置修订撤销旧浏览器权限。
4. 本机签发固定期限一次性配对码，H5 输入后仍无任务权限；回本机核对 exact principal、设备/config 修订、配对请求 ID 和 label，逐项选定会话/动作/任务限制/期限。不能通过 H5 管理模型、凭据、能力、备份或执行批准。
5. 撤权后关闭流并拒绝新输入。已接受桌面任务补充/回答不回滚、不迁移任务所有权；只有带 gateway 来源的新任务/继续任务持续受此授权约束。

网关数据库父目录应由专用服务账号私有持有；实现拒绝 symlink 数据库和启动后缺失的数据库，不声称抵御同权限恶意进程替换父目录的 ABA/VFS 攻击。传输 token 当前由两个独立 UUID v4 随机段组成 64 个小写十六进制字符，不把展示长度误称为完整 256 位随机熵。
