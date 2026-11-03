# 会话总结 — 2026-11-03 TCP 洞穿（Hole-Punching）协调机制

## 背景

在 chatx（基于 libp2p 的 P2P 即时通讯客户端）中，两个 NAT 后的客户端
往往无法直接 dial 到对方。此前已实现：

- 信令服务器可广播 Relay 地址（`GET /v1/relays` + relay 任务就绪后自动上报）
- 客户端可 STUN 发现公网 IP（`crates/core/src/stun.rs`）
- `endpoints_with_egress` 优先使用 STUN 公网 IP + 本机真实监听端口

遗留痛点：当双方都是对称 NAT / 无法直连时，消息会丢失。本次会话补充了
"唤醒 → 协调 dial" 机制。

## 设计

```
A ──POST /v1/notify?peer_id=B──▶ Server ◀──GET /v1/notifications?peer_id=B── B
       body: {"from":"A"}                                (long-poll ~45s)
```

两个 NAT 后的客户端：
- A 发起一次普通 DM dial 失败 → A 调 `wake_peer(B)`（best-effort）
- A 本地 sleep 3s（让 B 的 long-poll 有时间收到唤醒）
- A 用最新 endpoints 列表（可能 B 已经上线 / STUN 完成 / NAT 状态变化）重试
- B 端在 `start_notify_pump` 后台线程中收到 `Notice{from:A}`，调用
  `dial_peer_by_id(A)` 主动 dial A
- 双向同时 dial → NAT 映射同时打开 → 连接打通

## 改动清单

### `signaling/src/main.rs`
- 新增 `Notice { peer_id, from, at }`
- 新增 `Notifies` 结构 + `push()` + `wait_for(peer_id)`（基于
  `tokio::sync::broadcast`，容量 256，超时 45s）
- 新增 `App.notifies: Notifies` 字段
- 新增 `POST /v1/notify?peer_id=<target>` handler
- 新增 `GET /v1/notifications?peer_id=<me>` handler
- 新增 `NotifyQuery { peer_id }` 结构体
- 测试 `wake_notice_roundtrip`（A→B 唤醒 → B 长轮询收到）
- 测试 `wake_notice_timeout_when_nothing`（无通知 → 5s 内返回空）
- 测试服务器 `Notifies` 超时改为 1s 以便快速返回

### `crates/core/src/signal.rs`
- 新增 `Notice { peer_id, from, at }` 公开结构体
- `DirectoryClient` trait 新增：
  - `fn notify(&self, peer_id, from) -> bool`（默认 `false`）
  - `fn poll_notices(&self, peer_id) -> Vec<Notice>`（默认 `vec![]`）
- 新增 `poll_http_client()` — 50s 读超时的专用 ureq agent
- `HttpDirectory::notify` — `POST /v1/notify`
- `HttpDirectory::poll_notices` — `GET /v1/notifications`
- `InMemoryDirectory` / `NullDirectory` 保持默认实现（不改动）

### `crates/core/src/lib.rs`
- 新增 `wake_peer(peer_base58) -> bool` 方法
  - 30s 冷却（`wake_last: Mutex<HashMap<String, u64>>` 字段）
  - 调用 `dir.notify(peer, me)`
- 新增 `dial_peer_by_id(peer_base58)` — 已连接则 no-op，否则 `warm_dial_peer`
- 新增 `peer_connected_str(peer_base58) -> bool` 便捷检查
- 新增 `process_wakes() -> u32` — 调 `poll_notices` 并对每个 `notice.from`
  dial 一次，返回拨号数
- 新增 `start_notify_pump(self: &Arc<Self>)` — 后台 std::thread 无限循环
  `process_wakes()`
- `send_dm` 改造：
  - 保留原 Round-1（遍历 endpoints 发文本，等 8s ack）
  - 失败后 `wake_peer(peer)` + `sleep(3s)`
  - Round-2 用最新 endpoints 重发一次
  - 抽出 `commit_dm(peer, text)` helper 复用 push+return
- `wake_last` 字段加入 `Client<D>` struct + `from_parts` 初始化

### `apps/chat/src/lib.rs`
- 登录成功后调用 `client.start_notify_pump()`（紧跟 `start_presence_loop()`）
  带 OnceLock 保护，重复调用安全

## 测试状态

所有测试全绿：
- `chatx-core`（库）：23/23
- `chatx-core`（e2e）：15/15
- `chatx-signal`：8/8（含 2 个新唤醒测试）
- `cargo build --workspace`：通过

## 关键决策

| 决策 | 理由 |
|------|------|
| `broadcast` 而非 per-peer channel | 简洁；容量 256 足以覆盖并发 peer；广播到所有订阅者代价可接受 |
| 服务器长轮询 45s / 客户端 50s 读超时 | 兼容 15s presence tick；客户端留 5s 余量 |
| 唤醒无认证（`from == peer_id` 拒绝） | 唤醒本身无副作用，仅触发 dial；peer 已被目录级信任 |
| `wake_peer` 30s 客户端冷却 | 防止对不可达 peer 反复唤醒导致服务器广播风暴 |
| `send_dm` wake→sleep(3s)→retry | 让 B 有窗口拨号；比纯等 long-poll 返回快 |
| `wake_last` 用 `Mutex<HashMap>` 而非 tokio | 只在 `wake_peer` 同步调用中使用，避免 async 依赖 |
| 后台线程跑 pump 而非 tokio task | 与 `start_presence_loop` 风格一致；调用阻塞 HTTP 不阻塞 worker |

## 后续可改进

- Round-2 retry 目前仍只等 3s + 一轮 8s ack。可考虑用 `broadcast` 在本地
  维护"对端已拨回"事件，进一步减少等待
- `send_voice_cmd` / `send_image_cmd` / `send_file_cmd` 尚未加同样的
  wake+retry 逻辑（只有 text 通道）— 后续按相同模式补
- 服务器目前只保留一次唤醒（`Notifies::wait_for` 首次命中即返回）。并发
  peer 的连续唤醒会丢失 — 长期可用"per-peer 环形队列"替换
- 当前 `start_notify_pump` 用全局 `OnceLock` — 多 profile 场景下只有
  第一个生效，可改为 per-profile

## 文件总览

- `docs/code-improvements.md` — 项目改进清单
- `signaling/src/main.rs` — 服务器（含新增 notify 端点）
- `signaling/src/relay.rs` — Relay server
- `crates/core/src/signal.rs` — DirectoryClient trait + HttpDirectory
- `crates/core/src/stun.rs` — STUN 客户端（上轮会话已建）
- `crates/core/src/lib.rs` — Client（含 wake_peer / notify_pump / send_dm
  新流程 / endpoints_with_egress）
- `apps/chat/src/lib.rs` — GUI 入口（login 时启动 pump）
