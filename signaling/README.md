# chatx-signal

ChatX's directory / signaling server. Shipped as two independent binaries:

| Binary | Role | Protocol | Default port |
|---|---|---|---|
| `chatx-signal` | User/device directory, group signaling keys, relay registration, wake notifications, `/v1/stun` endpoint | HTTP (axum) + libp2p relay | TCP `8080` (HTTP), TCP/UDP `1883` (relay, optional) |
| `chatx-stun` | RFC 5389 STUN Binding server | UDP | UDP `3478` |

The two are **completely independent processes** with no shared runtime. STUN must run as a standalone process — embedding it in the signal server would starve the runtime and block `/v1/health` (a bug hit in early versions).

---

## 1. Build

```sh
# From the repository root
cargo build --release -p chatx-signal

# Artifacts
ls target/release/chatx-signal   # HTTP directory + relay service
ls target/release/chatx-stun     # STUN binding service
```

Or for a debug build:

```sh
cargo build -p chatx-signal
```

Unit tests:

```sh
cargo test -p chatx-signal
```

### Linux server package (x86_64)

To build an x86_64 Linux tarball for deployment — from **macOS or Linux** —
use `scripts/build_signal.sh`. It cross-compiles both binaries and bundles a
deployment `.env` + systemd units:

```sh
./scripts/build_signal.sh             # release, glibc  → build/signal/chatx-signal-x86_64linux-gnu-release.tar.gz
./scripts/build_signal.sh --musl      # musl build (self-contained, no host deps)
./scripts/build_signal.sh --debug     # faster compile

# macOS needs a Linux cross C toolchain first (glibc default):
brew tap messense/macos-cross-toolchains && brew install x86_64-linux-gnu
```

---

## 2. Environment variables

### `chatx-signal`

| Variable | Default | Description |
|---|---|---|
| `CHATX_SIGNAL_ADDR` | `0.0.0.0:8080` | HTTP directory service listen address |
| `CHATX_RELAY_ADDR` | `0.0.0.0:1883` | libp2p circuit relay listen address (set to `off` to disable relay) |
| `CHATX_RELAY_ADVERTISE` | — | Public `host[:port]` the relay advertises (required inside cloud VPCs; inferred from NIC otherwise) |
| `CHATX_STUN_ADVERTISE` | — | STUN `host:port` returned by `/v1/stun` (client dial target; keep consistent with `chatx-stun`'s `CHATX_STUN_ADVERTISE`) |
| `RUST_LOG` | `info` | Log level; logs go to `~/.chatx-signal/chatx-signal.log` |

**Data / log locations**:

- SQLite database: `~/.chatx-signal/signal.db`
- Runtime log: `~/.chatx-signal/chatx-signal.log`

### `chatx-stun`

| Variable | Default | Description |
|---|---|---|
| `CHATX_STUN_ADDR` | `0.0.0.0:3478` | STUN listen address (UDP) |
| `CHATX_STUN_ADVERTISE` | — | Public IP or hostname to advertise (required behind cloud VPC/NAT; otherwise detected via outbound probe) |
| `RUST_LOG` | `info` | Log level (stdout output, pipe as needed) |

---

## 3. Deployment

### 3.1 Single server (both processes on the same machine)

The most common setup — **signal server + STUN on the same public host**. Clients auto-learn the STUN address on this host via `GET /v1/stun`.

```bash
# Assume public IP is 203.0.113.10

# ─── signal server ───
CHATX_SIGNAL_ADDR=0.0.0.0:8080 \
CHATX_RELAY_ADDR=0.0.0.0:1883 \
CHATX_RELAY_ADVERTISE=203.0.113.10 \
CHATX_STUN_ADVERTISE=203.0.113.10:3478 \
RUST_LOG=info \
  ./chatx-signal

# ─── STUN (standalone process, start before signal) ───
CHATX_STUN_ADDR=0.0.0.0:3478 \
CHATX_STUN_ADVERTISE=203.0.113.10 \
RUST_LOG=info \
  ./chatx-stun
```

**Verify** (on the server itself or within the same network segment):

```sh
curl -s  http://203.0.113.10:8080/v1/health   # → "ok"
curl -s  http://203.0.113.10:8080/v1/stun     # → [{"addr":"203.0.113.10:3478"}]
curl -s  http://203.0.113.10:8080/v1/relays   # → [{"addr":"/ip4/203.0.113.10/tcp/1883/p2p/12D3KooW..."}]
```

> **`CHATX_RELAY_ADVERTISE` / `CHATX_STUN_ADVERTISE` must be set to a publicly reachable address.** Inside cloud VPCs, NIC IPs are private; both advertise env vars must be explicit. Otherwise the relay advertises a private IP and cross-network clients can't dial it; same for STUN.

### 3.2 Split deployment (STUN on edge/CDN, signal in the center)

If STUN is placed at low-latency edge and signal in a central datacenter (recommended at scale), point `CHATX_STUN_ADVERTISE` at the edge machine:

```bash
# Central datacenter (signal at 10.0.0.5, public 203.0.113.10)
# Note: CHATX_STUN_ADVERTISE points to the edge STUN, not the signal host
CHATX_SIGNAL_ADDR=0.0.0.0:8080 \
CHATX_STUN_ADVERTISE=edge.chatx.example:3478 \
  ./chatx-signal

# Edge machine (public 203.0.113.20)
CHATX_STUN_ADDR=0.0.0.0:3478 \
CHATX_STUN_ADVERTISE=203.0.113.20 \
  ./chatx-stun
```

Clients still only dial the signal server for `/v1/stun`, but actual STUN traffic goes to the edge machine.

### 3.3 systemd services (recommended for production)

`/etc/systemd/system/chatx-signal.service`:

```ini
[Unit]
Description=chatx signaling directory + relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=chatx
Environment=RUST_LOG=info
Environment=CHATX_SIGNAL_ADDR=0.0.0.0:8080
Environment=CHATX_RELAY_ADDR=0.0.0.0:1883
Environment=CHATX_RELAY_ADVERTISE=<PUBLIC-IP>
Environment=CHATX_STUN_ADVERTISE=<PUBLIC-IP>:3478
ExecStart=/opt/chatx/chatx-signal
Restart=always
RestartSec=3
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/chatx-stun.service`:

```ini
[Unit]
Description=chatx STUN binding server (standalone process)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=chatx
Environment=RUST_LOG=info
Environment=CHATX_STUN_ADDR=0.0.0.0:3478
Environment=CHATX_STUN_ADVERTISE=<PUBLIC-IP>
ExecStart=/opt/chatx/chatx-stun
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

Enable:

```sh
sudo cp chatx-{signal,stun}.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now chatx-stun chatx-signal
journalctl -u chatx-signal -e
```

> **Start `chatx-stun` before `chatx-signal`.** Clients call the STUN preheater on login; if STUN hasn't bound yet and signal is already accepting logins, the first batch of users will get STUN fingerprint failures.

### 3.4 Disabling relay (directory + STUN only)

If you already have an external relay (e.g. coturn), turn off the built-in relay:

```bash
CHATX_RELAY_ADDR=off \
CHATX_STUN_ADVERTISE=<PUBLIC-IP>:3478 \
  ./chatx-signal
```

---

## 4. Client configuration

ChatX clients locate the signal server in the following order (`resolve_default_url` in `crates/core/src/signal.rs`):

1. Environment variable `CHATX_SIGNAL`
2. `signal_url` in `~/.config/chatx/<profile>/config.toml` (auto-written with the default on first launch, editable)
3. Built-in default `https://signal.chatx.dev`

Point at a self-hosted signaling server:

```sh
# Pick one

# A. Environment variable (highest priority)
export CHATX_SIGNAL=http://203.0.113.10:8080
./Chatx.app/Contents/MacOS/chatx        # or the platform-specific client

# B. Edit config.toml
mkdir -p ~/.config/chatx
cat > ~/.config/chatx/config.toml <<'EOF'
signal_url = "http://203.0.113.10:8080"
EOF
```

On login, the client **automatically** calls `GET /v1/stun` and registers the returned STUN `host:port` as the preferred STUN server (see `Client::discover_stun` → `stun::set_custom_servers`). **No separate STUN configuration is needed on the client.**

> **Why prefer a local STUN**: public STUN servers (stun.l.google.com, etc.) may have UDP blocked in some networks (carrier NAT, mobile CGNAT, certain corporate egress). Co-locating STUN with signal on a client-reachable IP ensures NAT fingerprint discovery (EIM vs symmetric NAT) works even in those restricted environments, letting the client decide whether to use a relay or attempt hole-punching.

---

## 5. Ports & firewall

| Service | Protocol | Port | Direction | Notes |
|---|---|---|---|---|
| signal server | TCP | `8080` | inbound | HTTP API (directory, relay list, wake notifications) |
| relay | TCP | `1883` | inbound | libp2p circuit relay |
| relay | UDP | `1883` | inbound | libp2p QUIC |
| STUN | UDP | `3478` | inbound | Binding request/response |

Open all the ports above in your NAT/cloud security group. Set `CHATX_{SIGNAL,RELAY,STUN}_ADVERTISE` to public addresses (not internal NIC IPs), otherwise cross-network clients cannot reach them.

---

## 6. Operations

### Health check

```sh
curl -fsS http://<HOST>:8080/v1/health   # expected: "ok"
```

### Upgrading

Both binaries are stateless (except signal's SQLite). Simply replace the binaries and run `systemctl restart`. `chatx-signal` reads `signal.db` on startup to restore directory data.

### Logging

- `chatx-signal`: `~/.chatx-signal/chatx-signal.log` (level via `RUST_LOG`)
- `chatx-stun`: stdout (binding/answer events logged at `debug` with `RUST_LOG=info`; use `RUST_LOG=debug` for verbose request logging)

### Troubleshooting

| Symptom | Investigation |
|---|---|
| Client reports `signal server unreachable` | Is `/v1/health` reachable? Is `CHATX_SIGNAL_ADDR` correct? Is the port open? |
| Client learns STUN but fingerprint stays unknown | Is `CHATX_STUN_ADVERTISE` a public IP? Is the client's UDP egress being NAT-rewritten? Check `chatx-stun` logs for `STUN binding answered` |
| Client can't dial relay | Is `CHATX_RELAY_ADVERTISE` a public IP? Is the multiaddr returned by `curl /v1/relays` correct? |
| `/v1/health` intermittent timeouts (early versions) | Legacy STUN-embedded bug. Fixed by upgrading to the standalone STUN process version (current) |

---

## 7. Code structure

- `src/main.rs` — signal server entry point + HTTP router + relay startup
- `src/stun.rs` — standalone STUN binary (`[[bin]] name = "chatx-stun"`), RFC 5389 Binding implementation, **own `main`**
- `src/relay.rs` — libp2p circuit relay v2 + advertise logic
- `src/db.rs` — SQLite storage (users / devices / groups / group_key_slots / inbox)
- `src/migrations/` — schema creation scripts

`stun.rs` also exposes `pub fn start(addr, ready_tx)` for testing or embedding in other hosts; in production just run the `chatx-stun` binary directly.
