# ChatX 代码改进清单

> 每项处理完后将 `[ ]` 改为 `[x]`。按优先级从高到低处理。

---

## P0 — 严重（影响稳定性和正确性）

### 1. [ ] 无优雅关闭机制

**问题**: `swarm_loop` 无 `CancellationToken`，进程退出时 libp2p 连接不断开、心跳线程泄漏、`pending_acks` 永挂。logout 不清理已 spawn 的 task。

**位置**:
- `crates/core/src/swarm.rs:511` — `tokio::spawn(swarm_loop(...))` 无 shutdown 信号
- `crates/core/src/lib.rs:670-682` — presence heartbeat 是 detached `std::thread`，永不退出
- `apps/chat/src/lib.rs:1059-1100` — logout 仅设全局变量为 None，不清理 swarm
- 无 `impl Drop for Client`，无 `ctrl_c` handler

**修改方向**:
1. 引入 `tokio_util::sync::CancellationToken` 贯穿 `boot` → `swarm_loop`
2. 实现 `Client::shutdown()` — drain channel → unsubscribe gossip → close swarm → 停 presence thread
3. 主程序注册 `ctrl_c` 信号处理，触发 `shutdown`
4. logout 调用 `shutdown` 而非直接丢弃引用

**验收标准**: 进程退出后无残留 socket/listener/thread；logout 后再次 login 不产生重复 swarm

---

### 2. [ ] `pending_acks` 条目泄漏

**问题**: 应用侧 `tokio::time::timeout` 超时后，`pending_acks` 中 oneshot Sender 永远不释放。无 TTL 清理（对比 `pending_inbound_image` 有 60s TTL）。

**位置**:
- `crates/core/src/swarm.rs:572-575` — insert into `pending_acks`
- `swarm.rs:949,1024,1087,1132` — 仅 success 时 remove
- `swarm.rs:963-989` — 仅 OutboundFailure 时 remove
- `crates/core/src/lib.rs:1210,1300,1414,1496,1569` — 应用侧 timeout 后直接丢弃，不回传 swarm

**修改方向**:
1. `Running` 中为 `pending_acks` 增加 `created_at: Instant` 字段
2. 在 `swarm_loop` 每个循环迭代中做 `retain`，删除超过 30s 的条目
3. 或改用 `tokio::time::interval` 周期性清理（10s）

**验收标准**: 连续发送 100 条到不可达 peer 后，`pending_acks` 长度不为 100

---

### 3. [ ] Swarm loop 内阻塞 IO (Head-of-line blocking)

**问题**: 单 task 处理所有协议事件；`moment::serve` 在循环内做 `std::fs::read`（可达 64MiB），阻塞所有其他消息和 ACK。

**位置**:
- `crates/core/src/swarm.rs:1161-1183` — `RrEvent::Message{Blob}` handler
- `crates/core/src/moment.rs:122` — `std::fs::read`
- 同样模式：所有 `send_response` 内的序列化

**修改方向**:
1. 文件读取/大 payload 构造用 `tokio::task::spawn_blocking`，结果回传主循环发 response
2. 或将各 protocol 拆为独立 task（每个 protocol 一个 handler task + channel 投递）
3. 最小改动方案：在 `Blob` handler 中将 `serve().await` 改为 `spawn_blocking(move || serve_sync(...)).await`

**验收标准**: 同时发送 text 请求和 50MB blob 请求，text ACK 延迟 < 100ms

---

### 4. [ ] `unwrap()` / `expect()` 治理 (729 处)

**问题**: 生产路径大量 `.lock().unwrap()` / `.expect("...")`。任何一处 poisoned mutex 或 IO 错误直接 panic 崩溃整个进程。

**重灾区**:

| 文件 | unwrap 数 | 风险 |
|------|-----------|------|
| `apps/chat/src/lib.rs` | 150 | UI 线程崩溃 |
| `crates/sqlite/src/joins.rs` | 52 | 查询失败 |
| `crates/sqlite/src/social.rs` | 45 | 社交功能 |
| `signaling/src/db.rs` | 39 | 信令服务崩溃 |
| `crates/core/src/swarm.rs` | 31 | **P2P loop 崩溃** |
| `crates/media/src/session.rs` | 27 | 通话中断 |
| `crates/core/src/group.rs` | 16 | 加解密 panic |

**修改方向**:
1. **swarm loop 内** (最高优先): 所有 `.lock().unwrap()` → `match lock { Ok(g) => ..., Err(_) => { tracing::error!(...); continue; } }`
2. **group.rs 加密路径**: `.expect("32B key")` → 返回 `Result`，调用方处理
3. **UI 回调**: 保留 `unwrap` 但确保上游已验证；或改 `if let Some(client) = CLIENT.lock().ok().and_then(|g| g.clone())`
4. 逐步推进，不要求一次改完 729 处；按模块分批
5. 建议：`#[cfg(not(test))]` 下将 `.expect("msg")` 替换为 `.unwrap_or_else(|e| { tracing::error!(err=%e, ...); /* fallback */ })`

**验收标准**: `swarm.rs` 中 unwrap 数 = 0；`group.rs` = 0；其余 < 50%

---

## P1 — 高（影响性能和可靠性）

### 5. [ ] Channel 背压控制

**问题**: `cmd_tx` / `evt_tx` 都是 `unbounded_channel()`，每个 `ChatEvent::Image/Voice/File` 携带 64MiB `Vec<u8>`。突发场景内存无限增长。

**位置**:
- `crates/core/src/swarm.rs:459-461` — `mpsc::unbounded_channel()`
- `swarm.rs:282,302` — `Running.cmd_tx`, `Running.injected`
- `ChatEvent::Image { data: Vec<u8> }` / `Voice` / `File` 变体

**修改方向**:
1. `evt_tx` 改为 `mpsc::channel(128)`，`cmd_tx` 改为 `mpsc::channel(64)`
2. 大 payload 改为 `Arc<[u8]>` 共享，避免 channel 内多次持有
3. 生产者侧如果 send 失败（channel 满）则 `tracing::warn` 并丢弃最旧事件
4. `apps/chat/src/lib.rs:2842,2853` 的 `data.clone()` 改为 `Arc::clone(&data)`（需改 ChatEvent 类型）

**验收标准**: 连续注入 100 条 64MiB image event，进程 RSS < 2GB（之前无上限）

---

### 6. [ ] 消息去重 (DM + P2P 群组)

**问题**: DM 用 `rand::random::<u64>()` 做 id，接收端不按 id 去重。gossipsub 可能重复投递同一 group packet。仅 server-mode group 有 O(N) 线性扫描去重（且对 media 无效）。

**位置**:
- `crates/core/src/swarm.rs:570,599,624,649,675,717,744` — id 生成
- `crates/core/src/lib.rs:2415-2443` — `process_event` 直接写 store，无 dedup
- `lib.rs:2490-2502` — 仅 server-mode group 有去重（且 O(N)）

**修改方向**:
1. **发送端**: 改用 `AtomicU64` 做 per-peer monotonically-increasing seq（或 `uuid` + seq 组合）
2. **接收端**: 维护 LRU set (`dashmap::DashSet` 或 `parking_lot::Mutex<HashSet<u64>>`)，容量 1024，超过则丢弃最旧
3. 在 `process_event` 入口检查 dedup set，重复则跳过
4. P2P group 同理：`GroupPacket.data` 前 8 字节作为 id hash 去重

**验收标准**: 模拟 gossipsub 重复投递，UI 不出现重复消息

---

### 7. [ ] DM 离线队列 + 重连补发

**问题**: `send_dm` 失败后消息直接丢弃。数据库 `offline_messages` 表存在但无生产代码调用。群组有 server inbox 轮询，DM 没有。

**位置**:
- `crates/core/src/lib.rs:1153-1238` — `send_dm` 失败即放弃
- `crates/sqlite/migrations/0001_baseline.sql:235-245` — `offline_messages` 表（dead code）
- `crates/sqlite/src/server.rs:31-75` — `enqueue/pending_for/mark_delivered` API（仅测试调用）
- 信令服务器 `signaling/src/main.rs:434-508` — 仅有 group inbox，无 DM inbox

**修改方向**:
1. `send_dm` 失败时调用 `offline_messages::enqueue(pool, ...)`
2. 后台 task 监听 `Connected` 事件 → `pending_for(peer)` → 逐条重发 → `mark_delivered`
3. 指数退避: 1s, 2s, 4s, 8s... 最大 120s；超过 6 次标记 `failed`
4. UI 侧：status=0 (sending) 展示发送中，重连后自动消失
5. 可选：信令服务器增加 DM inbox（类似 group inbox），离线 peer 上线后从 server 拉取

**验收标准**: 断网发 5 条消息 → 恢复网络 → 5 条全部送达 peer

---

### 8. [ ] 异步上下文中阻塞调用

**问题**:
- `warm_dial_peer` 使用 `std::thread::sleep`（累计可达 18s）从同步上下文调用
- `store_group_outbound` 路径中 `std::fs::write`（64MiB）在 async fn 中执行
- App pump 中 `handle_inbound_image/voice/file` 做 `std::fs::write` + `data.clone()` (64MiB)

**位置**:
- `crates/core/src/lib.rs:289,293` — `std::thread::sleep(POLL)` in `warm_dial_peer`
- `lib.rs:1932,1950,1974` — `std::fs::write` in `write_group_media`
- `lib.rs:2568-2572` — `std::net::UdpSocket` probe in `current_egress_ip`
- `apps/chat/src/lib.rs:2838-2856` — `data.clone()` 64MiB in pump
- `apps/chat/src/record.rs:169,247` — `std::fs::write` in voice handler

**修改方向**:
1. 所有 `std::fs::write/read` 在 async 上下文中 → `tokio::task::spawn_blocking(move || std::fs::write(...))`
2. 或直接用 `tokio::fs::write`
3. `warm_dial_peer`: 改为 async fn，用 `tokio::time::sleep` 替换 `std::thread::sleep`
4. `data.clone()` → `Arc<[u8]>`（配合 #5 一起改）
5. `current_egress_ip`: 移到 spawn_blocking 或使用异步 UDP

**验收标准**: `cargo clippy -- -W blocking-in-async` 无警告（或已有 allow 的加注释说明）

---

### 9. [ ] `#[instrument]` 结构化追踪

**问题**: 119 处 `tracing::debug!/warn!/info!` 但零 `#[instrument]`。无法做 parent-child span 关联和函数级耗时归因。

**关键函数**:

| 函数 | 位置 |
|------|------|
| `swarm_loop` | `swarm.rs:516` |
| `process_event` | `lib.rs:2412` |
| `send_dm` | `lib.rs:1153` |
| `send_voice_cmd` | `lib.rs:1280` |
| `send_image_cmd` | `lib.rs:1398` |
| `seal_message` / `open_message` | `group.rs` |
| `heartbeat` | `lib.rs:664-682` |
| `poll_group_server_inbox` | `lib.rs:2330` |
| `handle_inbound_image` | `apps/chat/src/lib.rs` |

**修改方向**:
1. 对上述函数添加 `#[tracing::instrument(skip_all)]` 或 `#[tracing::instrument(fields(peer_id = %xxx))]`
2. 在 `Client::bootstrap` 中设根 span `trace!("client bootstrap")`
3. 配合 `RUST_LOG=chatx=debug` 可追踪完整消息生命周期

**验收标准**: `RUST_LOG=debug` 下能看到一条 DM 从发送到接收的完整 span tree

---

## P2 — 中（影响用户体验和可维护性）

### 10. [ ] `next_event` 强制单消费者

**问题**: `events: tokio::sync::Mutex<EventRx>` 导致所有事件消费者串行化；`next_event()` 持有 Mutex guard 跨 `.recv().await`。

**位置**: `crates/core/src/lib.rs:34,1606-1611`

**修改方向**: 用 `tokio::sync::broadcast` channel 替代 Mutex+mpsc；或 `flume::bounded` + 多 subscriber

---

### 11. [ ] `groups` 锁竞争 (writer starvation)

**问题**: `parking_lot::RwLock<BTreeMap<String, Group>>` 在 `apply_inbound`(含 HTTP `resolve_group`) 期间持有读锁，慢网络 + 写者到达 = reader 饿死 UI。

**位置**: `lib.rs:35`, `apply_inbound:1624-1693`, `save_group:2398-2402`

**修改方向**: 将 `resolve_group` (HTTP) 移到锁外；或拆 groups 为 per-group mutex；或改 actor model

---

### 12. [ ] 全局 `std::sync::Mutex` 三件套

**问题**: `CLIENT` / `POOL` / `BACKEND` 三个全局 `std::sync::Mutex<Option<Arc<...>>>`，每个 Slint callback 都 lock。

**位置**: `apps/chat/src/lib.rs:39-42`

**修改方向**: 改用 `arc_swap::ArcSwap`（无锁 reads）或 `once_cell::sync::Lazy` + `parking_lot::RwLock`；logout 时 swap 而非 None

---

### 13. [ ] 7 个独立轮询器统一

**问题**: 多个独立定时器 (2s, 1s, 15s, 60s, 16ms, 100ms, 150ms) 各打同一 `Client` 对象。

**位置**: 见各轮询表

**修改方向**: 统一为 `tokio::time::Interval` + 事件驱动替代（如 presence 改 push notification）；合并相近周期

---

### 14. [ ] `send_response` 失败静默丢弃

**问题**: `let _ = swarm.behaviour_mut().chat.send_response(channel, resp);` 无日志无指标。

**位置**: `swarm.rs:922-923,1011-1012,1098,1122,1181`

**修改方向**: 改 `if let Err(e) = ... { tracing::warn!(?e, "send_response failed"); }`

---

### 15. [ ] Unsafe FFI 无校验

**问题**: `bridge/src/ffi.rs` 中 `std::slice::from_raw_parts` 无 length/alignment 验证，传入垃圾指针即 UB。

**位置**: `crates/bridge/src/ffi.rs:46-149`

**修改方向**: 所有 `from_raw_parts` 前置 `assert!(!ptr.is_null() && len <= MAX && (ptr as usize) % 8 == 0)`；或提供 safe wrapper trait

---

### 16. [ ] `crates/network` 空 stub

**问题**: workspace 成员但仅一行 `add(u64,u64)`，无实际功能。

**位置**: `crates/network/src/lib.rs`

**修改方向**: 实现或删除（`cargo remove-member`）

---

## P3 — 缺失功能（按用户价值排序）

### 高优先级

### 17. [ ] DM 已读回执 (delivered / read status)

**现状**: status 列 (0-4) 和 `set_status` API 存在，但 `delivered`(2) 和 `read`(3) 从不被设置。

**修改方向**:
1. 接收端 `process_event` 成功后回发 `ReadReceipt { msg_id, read: bool }`
2. 或复用现有 `ChatResponse.ack` 作为 delivered；新增 `ChatRequest { kind: ReadReceipt }` 作为 read
3. UI 侧：消息气泡右下角 ✓ (delivered) → ✓✓ (read)

---

### 18. [ ] 输入中状态 (typing indicator)

**现状**: 完全缺失。无 wire message kind，无 UI 状态。

**修改方向**:
1. `ChatRequest` 增加 `kind: Typing { is_typing: bool }`
2. 发送频率限制 (每 3s 最多一次)
3. UI: header 显示 "对方正在输入…"，3s 后消失
4. 群组: 仅显示 "某人正在输入…"（不泄露具体文本）

---

### 19. [ ] 消息撤回 (远端生效)

**现状**: 本地 `delete` 存在但仅删 DB 行，不发 wire 命令，peer 不受影响。UI 无 long-press → 撤回 入口。

**修改方向**:
1. 新增 `ChatRequest { kind: Recall { original_id } }`
2. 接收端收到后按 `original_id` 标记 `recall = true`（不删行，保留占位）
3. UI: 灰底显示 "该消息已撤回"
4. 时间限制: 2 分钟内可撤回

---

### 中优先级

### 20. [ ] 消息编辑

**现状**: 完全缺失。

**修改方向**: 类似 recall，新增 `Edit { original_id, new_text }`；UI 长按 → 编辑；消息下方显示 "已编辑" 标记

---

### 21. [ ] 用户封禁 (block list)

**现状**: mute/pin 已实现，block 完全缺失。无 table/API/UI。

**修改方向**:
1. 新表 `blocked_users (user_id, blocked_at)`
2. `process_event` 入口检查 blocked set → 直接 drop
3. 目录服务器: 发消息前检查 block
4. UI: 联系人详情 → "屏蔽此人"

---

### 22. [ ] 历史消息分页加载

**现状**: `load(chat_id, 200, 0)` 硬限 200 条，无 "scroll to top → load older" 逻辑。

**位置**: `apps/chat/src/lib.rs:4362`

**修改方向**:
1. Slint `Flickable` 触顶时触发 `load_older(chat_id, offset, 100)`
2. 插入到消息列表头部，保持 scroll position
3. 加 loading indicator

---

### 23. [ ] WebRTC ICE restart + 自动恢复

**现状**: `RTCPeerConnectionState::Failed` 后仅 tear-down，无 ICE restart。

**位置**: `crates/media/src/session.rs:617-640`

**修改方向**:
1. `state == Failed` → 调用 `RTCRtpSender::setICEgatheringState` + `createOffer({ iceRestart: true })`
2. 最多 3 次 ICE restart，间隔 2s
3. 全部失败后再 tear-down

---

### 低优先级

### 24. [ ] 聊天导出/备份

**现状**: 完全缺失。

**修改方向**: UI 设置 → "导出聊天记录" → 生成 JSON/EML 文件；或 SQLite `.dump` 压缩为 `.zip`

---

### 25. [ ] 速率限制/反垃圾

**现状**: 完全缺失。

**修改方向**:
- Client: sliding window (10 msg/5s)，超出则 drop + toast
- Server: 按 user_id/peer_id token bucket
- 群组: per-member rate limit

---

### 26. [ ] 内容举报/审核

**现状**: 完全缺失。

**修改方向**:
1. 新表 `reports (id, reporter_id, target_user_id, target_msg_id, reason, status)`
2. UI: 长按消息 → "举报"
3. 管理端 API (可选)

---

### 27. [ ] 协议协商/能力交换

**现状**: 静态版本号 `/chatx/text/1`，无 handshake/caps 交换。

**修改方向**: 连接后先发 `Hello { caps: [String], version: u32 }`，双方取交集决定支持的功能集

---

### 28. [ ] WASM 小程序完善

**现状**: 仅 `hello` demo + host + SDK basics。5 个 viewer plugin (pdfview/sheetview/docview/pptview/map) 未实现，`x/x-m` 是 3 行 stub。

**位置**: `x/`, `crates/wasmhost/`

**修改方向**: 按 `x/README.md` 规划逐步实现 plugin + distribution server

---

## 附加清理

### 29. [ ] 硬编码值可配置化

**位置**: `swarm.rs` (timeout/backoff), `signal.rs` (TTL), `lib.rs` (per-attempt timeout), `sqlite/lib.rs` (busy_timeout)

**修改方向**: 提取到 `Config` struct，支持 env var + TOML 配置文件覆盖

### 30. [ ] `tokio` feature 裁剪

**位置**: workspace `Cargo.toml`

**修改方向**: library crate 仅启用所需 features（`rt-multi-thread`, `net`, `sync`, `time`），去掉 `signal`, `process` 等

---

## P0-NAT — 外网连接问题（NAT 穿透）— 高优先

> **已知问题**：libp2p 节点没有上报外网 IP，导致外网节点连接不上，只能连局域网节点。
> 根因：本进程没有任何公网 IP 发现机制；`current_egress_ip()`（`lib.rs:2631`）
> 返回的是私有 LAN IP（通过 8.8.8.8 选 egress 接口的 `local_addr`），却被当作
> "公网地址" 上报到目录。UPnP 事件全丢弃、Relay 需手动配 `CHATX_RELAYS`、无 STUN/打洞。

### 31. [x] ① 信令服务器 `GET /v1/relays` 端点

**问题**: Relay 地址只出现在 server 启动日志和 `CHATX_RELAYS` env 里，客户端无法自动发现。

**位置**: `signaling/src/main.rs`, `signaling/src/relay.rs`

**已实现**:
- `relay.rs` 新增 `start(addr, ready_tx)`，监听就绪后把完整 advertise multiaddr
  （`/ip4/host/tcp/port/p2p/<relay_peer>`）经 `ready_tx` 上报
- `App` 增加共享 `relays: Arc<tokio::sync::Mutex<Vec<String>>>`
- `main()` 启动 relay 任务后把 advertise 写入 `relays`
- 新增 `GET /v1/relays` → 返回 `Vec<RelayInfo{addr}>`
- 新增测试 `relays_endpoint_returns_empty_list`

**状态**: ✅ 完成

### 32. [x] ② 客户端自动发现 relay

**问题**: `CHATX_RELAYS` 未设时，外网无任何穿透回退。

**位置**: `crates/core/src/signal.rs`, `crates/core/src/swarm.rs`, `crates/core/src/lib.rs`

**已实现**:
- `signal.rs` 新增 `RelayInfo` 结构 + `DirectoryClient::list_relays`（默认空）+
  `HttpDirectory` 实现（`GET /v1/relays`）
- `swarm.rs` 新增 `parse_relay_strings(&[String]) -> Vec<(PeerId, Multiaddr)>`
- `lib.rs` 将 `reconnect_groups` 拆分出 `reserve_relays()`；新增
  `discover_relays()`：若 `CHATX_RELAYS` 为空则从目录拉取并合并、按需 reserve
- `refresh_directory_auto`（登录/bootstrap 统一入口）在注册前调用 `discover_relays()`
- `CHATX_RELAYS` 仍保持最高优先级（已配置则跳过服务器发现）
- 新增测试 `swarm::tests::parse_relay_strings_*`

**状态**: ✅ 完成

### 33. [x] ③ UPnP/relay 地址确认后立刻上报

**问题**: UPnP 公网映射或 relay reservation 成功后，最坏要等 15s 周期心跳才上报目录。

**位置**: `crates/core/src/swarm.rs`, `apps/chat/src/lib.rs`

**已实现**:
- `ChatEvent` 新增 `ExternalAddrConfirmed { address }` 变体
- swarm loop 在 `ExternalAddrConfirmed` 去重后向 app 发出该事件（去重，避免重复触发）
- `apps/chat/src/lib.rs` pump 收到该事件即在独立 blocking task 上 `heartbeat()`
  （`AtomicBool` 去抖，UPnP+relay 同时确认只触发一次）

**状态**: ✅ 完成

### 34. [x] ④ 全私有端点的诊断日志

**问题**: 节点只上报私有 IP 且无 relay 时，用户无任何提示，"卡在外网连不上"。

**位置**: `crates/core/src/lib.rs`

**已实现**:
- 新增 `is_routable_public_or_circuit(&str) -> bool`（识别公网 IPv4/IPv6 或 `/p2p-circuit`）
- `heartbeat()` 状态翻转式诊断：进入"全私有地址"态 `warn` 一次，恢复公网/relay 后 `info` 一次，避免每 15s 刷日志

**状态**: ✅ 完成

### 35. [x] Phase 2 — STUN 公网 IP 发现

**问题**: `current_egress_ip()` 返回私有 IP，被当作公网地址上报；节点真实公网 IP 仍未知。

**位置**: `crates/core/src/stun.rs`（新增）, `crates/core/src/lib.rs`（`endpoints_with_egress` + `start_presence_loop`）

**已实现**:
- 新增 `crates/core/src/stun.rs`（RFC 5389 STUN binding，IPv4 + IPv6，
  MAPPED + XOR-MAPPED，多服务器 fallback，120s 进程级缓存，
  含 `PublicIp` / `discover()` / `discover_cached()` API）
- `endpoints_with_egress` 改造：优先 STUN 公网 IP（配本机真实监听端口），
  无 STUN 时回退原 LAN egress 逻辑
- `start_presence_loop` 新增 prewarm 线程（非阻塞 STUN 探测 + 立即心跳），
  确保首次上报即带公网地址
- 新增 5 个单元测试（含真实网络 live probe）

**状态**: ✅ 完成

### 36. [x] Phase 2 — 信令辅助 TCP hole-punching ✅

**问题**: 双方都 dial 不到对端时没有第三条路（无同时打洞协调方）。

**已做**:
1. 信令新增 `POST /v1/notify?peer_id=<B>` + `GET /v1/notifications?peer_id=<B>`
   long-poll 队列（45s server / 50s client 读超时）
2. 客户端 `wake_peer` 带 30s/peer 冷却防 server 负载；`process_wakes` 收到后
   立即 dial 回对端
3. `send_dm` 路径：wake_peer → sleep(3s) → 重试 dial；仍失败回退到 #32 relay
4. 新增 2 个 signal 单测覆盖 notify/notifications 队列

### 37. [x] Phase 3 — 启用 QUIC/UDP 传输 ✅

**问题**: 仅 TCP，无法 UDP 打洞 / 连接迁移。

**位置**: `crates/core/src/swarm.rs` (`boot`)、`signaling/src/relay.rs` (`start`)

**已做**:
- libp2p `quic` feature 已启用（workspace `Cargo.toml`）
- 客户端 `SwarmBuilder` 链追加 `.with_quic()`，并 listen 第二条 `/ip4/0.0.0.0/udp/0/quic-v1`
- relay 服务端 `SwarmBuilder` 链追加 `.with_quic()`，listen `/ip4/{ip}/udp/{port}/quic-v1`
  （与 TCP 同端口共用 `CHATX_RELAY_ADDR`，部署只需放行一条 UDP 端口）
- QUIC multiaddr 必须带 `/quic-v1` 后缀，仅 `/udp/...` 会被拒绝
- 测试：core 23 / e2e 15 / signal 8 全绿

### 38. [x] Phase 0 — relay advertise 公网地址 & reservation 握手 ✅

**问题**: relay 有两个相互独立的 bug 导致跨网段 `/p2p-circuit` 建不起来：
1. relay 默认绑定 `0.0.0.0:1883`，advertise 出去的是
   `/ip4/0.0.0.0/tcp/1883/p2p/...` —— 任何跨网段客户端都拨不通
2. 更隐蔽：relay server 从未 `add_external_address`，`swarm.external_addresses`
   为空 → relay v2 协议里 reservation 成功响应的 `addrs` 为空 → 客户端
   报 `NoAddressesInReservation` 直接关掉 circuit listener → 目录永远没有
   `/p2p-circuit`（这就是"server reservation accepted 但客户端连不上"的根因）

**位置**: `signaling/src/relay.rs`（advertise + external address 注册）

**已做**:
- relay 启动时 `resolve_public_host()`：优先 `CHATX_RELAY_ADVERTISE=<公网host:port>`
  （支持 IP 或域名 DNS 解析），其次出站 UDP 探测拿 local IP，最后回退保留原地址并 `warn!`
- announce 时 `with_public_host()` 把 listen 地址的 IP 替换成公网 host，
  保留原始 transport（`/tcp/` 或 `/udp/...quic-v1`）
- **关键**：`swarm.add_external_address(pub_tcp/pub_udp)` 让 relay 在 reservation
  响应里能带回地址，客户端 `NoAddressesInReservation` 不再触发，`/p2p-circuit`
  正常进入地址簿与目录
- `swarm.rs` `Cmd::ReserveRelay`：显式 `add_peer_address(relay, base)` 确保客户端
  能拨到 relay（不要 `swarm.dial`，那会破坏 relay 内部的 reservation 握手）
- 新增 signal 单测覆盖 TCP/UDP advertise / 回退，及真实 `/v1/relays` 格式解析

**部署**: relay 启动时设置 `CHATX_RELAY_ADVERTISE=<公网host:port>`
（如 `139.159.201.87:1883`）。未设置时 relay 仍可出站探测，但探测到私有 IP
会 `warn!` 且 reservation 可能失败。

### 39. [x] Phase 3 — relay→直连升级（§A）✅

**问题**: 连接一旦走上 relay 就一直走 relay。即使对端后来暴露了直连地址
（换到同一 Wi-Fi / NAT 可打洞），chatx 也不会升级，白白消耗 relay 带宽与延迟。

**位置**: `crates/core/src/swarm.rs`

**已做**:
- 监听 `NewExternalAddrOfPeer`：当对端新广播一个**非 relay**（无 `/p2p-circuit`）
  地址、且本端当前已与该 peer 有连接时，触发重新 `dial` —— libp2p 重新并发
  竞争所有已知地址，从而优先选到直连路径
- 新增 `is_relayed(addr)` 助手判断地址是否含 `/p2p-circuit`
- `Running.escalation_last` 记录每 peer 最近升级时间，30s 冷却防抖，避免
  地址抖动时反复重拨
- 新增单测 `is_relayed_detects_circuit_only`

**说明**: 不依赖 QUIC connection migration / DCUtR（libp2p-quic 当前
`migration(false)`，开启需 patch 库）。§A 用应用层"新地址到达即重 dial"实现，
改动小、风险低，已能满足 relay→直连升级。

---

## 实施路线图

```
Phase 0 — 外网连通 (NAT 穿透)
  P0-NAT-31 ① 信令 GET /v1/relays            ✅ 完成
  P0-NAT-32 ② 客户端自动发现 relay            ✅ 完成
  P0-NAT-33 ③ UPnP/relay 确认后立即上报       ✅ 完成
  P0-NAT-34 ④ 全私有端点诊断日志              ✅ 完成
  P0-NAT-35 ⑤ STUN 公网 IP 发现              ✅ 完成
  P0-NAT-36 ⑥ 信令辅助 TCP hole-punching     ✅ 完成
  P0-NAT-37 ⑦ 启用 QUIC/UDP 传输              ✅ 完成
  P0-NAT-38 ⑧ relay advertise 公网地址+诊断    ✅ 完成

  ⇒ Phase 0 全部交付：自动发现 relay、UPnP/relay 确认立即上报、无公网地址
     诊断日志、STUN 公网 IP 发现、信令辅助 TCP hole-punching、QUIC/UDP 传输、
     relay advertise 可拨号公网地址。外→外连接在有内置 relay 的场景下已可打通
     （无需 CHATX_RELAYS），并具备 UDP 直连 + hole-punching 能力。

Phase 1 — 稳定性 (Week 1-2)
  P0-1  优雅关闭
  P0-2  pending_acks 泄漏
  P0-3  swarm loop 阻塞 IO
  P0-4  unwrap 治理 (swarm + group 优先)

Phase 2 — 可靠性 (Week 3-4)
  P1-5  Channel 背压
  P1-6  消息去重
  P1-7  DM 离线队列
  P1-8  异步阻塞调用
  P1-9  #[instrument]

Phase 3 — 体验 (Week 5-6)
  P3-17 已读回执
  P3-18 输入中状态
  P3-19 消息撤回
  P3-22 历史分页
  P3-21 用户封禁

Phase 4 — 架构优化 (Week 7-8)
  P2-10 多消费者
  P2-11 groups 锁拆分
  P2-12 全局 Mutex 治理
  P2-13 轮询统一
  P2-14 send_response 日志
  P2-15 FFI 安全
  P3-23 WebRTC 恢复
```
