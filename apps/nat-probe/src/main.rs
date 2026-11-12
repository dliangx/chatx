//! `chatx-nat-probe` — one-shot NAT diagnostic for this host.
//!
//! What it reports:
//!   1. Per-family NAT fingerprint (EIM / EDM / Unknown) + public IP — via the
//!      dual-socket STUN probe from `chatx_core::stun` (one for IPv4, one
//!      for IPv6, when both address families are present).
//!   2. UDP egress reachability (needed by STUN, QUIC, and any direct punch).
//!   3. Listening sockets (informational — the real ports are logged by the
//!      app at startup).
//!   4. Relay reachability, if `CHATX_RELAYS` is set — the relay must be
//!      reachable for BOTH signaling and DCUtR coordination.
//!   5. The cell(s) of the hole-punch success matrix this host is likely in.
//!
//! Usage:
//!   cargo run -p chatx-nat-probe
//!   CHATX_RELAYS=/ip4/1.2.3.4/tcp/1883/p2p/Qm... chatx-nat-probe

use std::net::UdpSocket;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    println!("=== chatx-nat-probe ===");
    println!();

    // 1. STUN fingerprint (per address family).
    println!("[1] STUN fingerprint (dual-socket x two-server, per family)");
    let profiles = tokio::task::spawn_blocking(|| chatx_core::stun::discover())
        .await
        .map_err(|e| anyhow::anyhow!("STUN task panicked: {e}"))?;
    let profiles = match profiles {
        Ok(p) => p,
        Err(e) => {
            println!("  STUN error: {e}");
            Vec::new()
        }
    };
    let note: &str = if profiles.is_empty() {
        println!("  no public IP (UDP egress blocked / offline / not yet reachable)");
        "none"
    } else {
        "ok"
    };
    if profiles.is_empty() {
        // No profiles — keep going with the rest of the report.
    } else {
        for p in &profiles {
            println!("  public IP   : {}", p.ip);
            println!("  mapping     : {}", p.mapping);
            println!("  stable-public: {}", p.is_stable_public());
            println!();
        }
    }

    // 2. UDP egress.
    println!();
    println!("[2] UDP egress round-trip (5s timeout to 8.8.8.8:53)");
    {
        use std::time::Duration;
        let sock = UdpSocket::bind("0.0.0.0:0").or_else(|_| UdpSocket::bind("[::]:0"));
        match sock {
            Ok(sock) => {
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = sock.set_write_timeout(Some(Duration::from_secs(5)));
                let dummy = [0u8; 1];
                match sock.send_to(&dummy, "8.8.8.8:53") {
                    Ok(_) => {
                        let mut buf = [0u8; 2];
                        match sock.recv_from(&mut buf) {
                            Ok(_) => println!("  round-trip OK — egress allowed"),
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                println!("  round-trip TIMEOUT — egress blocked / server down");
                            }
                            Err(e) => println!("  round-trip FAILED: {e}"),
                        }
                    }
                    Err(e) => println!("  UDP send failed: {e}"),
                }
            }
            Err(e) => println!("  bind failed: {e}"),
        }
    }

    // 3. Listening sockets (informational).
    println!();
    println!("[3] Listening sockets (netstat)");
    list_listeners();

    // 4. Relay reach.
    println!();
    println!("[4] Relay reach");
    let relays = std::env::var("CHATX_RELAYS").unwrap_or_default();
    if relays.trim().is_empty() {
        println!("  CHATX_RELAYS not set — skipping (LAN / direct-only).");
    } else {
        for piece in relays.split_whitespace() {
            print_relay_reach(piece).await;
        }
    }

    // 5. Matrix position (per family).
    println!();
    println!("[5] Hole-punch success matrix (from OUR side, per family)");
    if profiles.is_empty() {
        println!("  not enough info — run with network access; or set CHATX_RELAYS and try the relay path.");
    } else {
        for prof in profiles {
            print_matrix(prof);
            println!();
        }
    }

    println!();
    println!("=== done ===");
    let _ = note;
    Ok(())
}

fn list_listeners() {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(
            "(netstat -an 2>/dev/null | awk '$6==~/(LISTEN|LISTENING)/' | head -20) \
             || (lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | head -20) \
             || (echo \"no listener tool available\")",
        )
        .output();
    match out {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            for line in s.lines().take(15) {
                println!("  {line}");
            }
        }
        Err(e) => println!("  listing failed: {e}"),
    }
}

async fn print_relay_reach(spec: &str) {
    let base: String = spec.split("/p2p/").next().unwrap_or(spec).to_string();
    // Parse /ip4/x.y.z.w/tcp/PORT — port is the last numeric segment.
    let segments: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    let mut ip: Option<std::net::IpAddr> = None;
    let mut port: Option<u16> = None;
    for s in &segments {
        if let Ok(v4) = s.parse::<std::net::Ipv4Addr>() {
            ip = Some(v4.into());
        }
        if let Ok(v6) = s.parse::<std::net::Ipv6Addr>() {
            ip = Some(v6.into());
        }
        if let Ok(p) = s.parse::<u16>() {
            port = Some(p);
        }
    }
    let started = std::time::Instant::now();
    let (ip, port) = match (ip, port) {
        (Some(i), Some(p)) => (i, p),
        _ => {
            println!("  {spec}: could not parse IP/port, skipping");
            return;
        }
    };
    let target: std::net::SocketAddr = (ip, port).into();
    let res = tokio::task::spawn_blocking(move || {
        std::net::TcpStream::connect_timeout(&target, std::time::Duration::from_secs(4))
    })
    .await;
    match res {
        Ok(Ok(_)) => println!("  reachable  {ip}:{port}  ({:?})", started.elapsed()),
        Ok(Err(e)) => println!("  UNREACHABLE {ip}:{port}  ({:#})  — relay is not usable; DCUtR + signaling need a reachable relay", e),
        Err(e) => println!("  join error: {e}"),
    }
}

fn print_matrix(p: chatx_core::stun::NatProfile) {
    match p.mapping {
        chatx_core::stun::Mapping::Eim => {
            println!("  us=EIM (cone)");
            println!("    vs Full Cone        -> OK");
            println!("    vs Restricted       -> OK");
            println!("    vs Port-Restricted  -> OK (strict timing); DCUtR handles this");
            println!("    vs Symmetric        -> RISKY (peer reports port we won't send to)");
            println!("  => hole-punching should work in 3 of 4 common combinations");
        }
        chatx_core::stun::Mapping::Edm => {
            println!("  us=EDM (symmetric)");
            println!("    vs Full Cone        -> OK (Full Cone accepts anything)");
            println!("    vs Restricted       -> RISKY (peer filter may reject our punch)");
            println!("    vs Port-Restricted  -> usually FAILS (we send to peer's STUN port, peer expects a different one)");
            println!("    vs Symmetric        -> FAILS (both sides get different ports)");
            println!("  => hole-punching rarely works; use the relay or TURN / port-map");
        }
        chatx_core::stun::Mapping::Unknown => {
            println!("  unknown — fingerprinted from 2 STUN servers, could not disambiguate.");
            println!("  likely multi-homed / CGNAT / split egress. Treat conservatively: relay-only or TURN.");
        }
    }
}
