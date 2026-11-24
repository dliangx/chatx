use crate::CLIENT;

// Group messages are now dual-delivered (live gossipsub + server inbox), so
// the old "10-minute grace / 15-second sweeper" bookkeeping that guarded the
// P2P-only model is gone. We keep a light per-group "already dialed" memo so
// `group_conn_enter` doesn't re-dial every member on every chat switch — but
// there is no periodic sweep and no forced disconnect; libp2p's own
// connection state (and the dual-delivery fallback) owns reachability.

#[derive(Clone, Copy)]
struct GroupConnSlot {
    last_enter_ms: u64,
}

static GROUP_CONNS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, GroupConnSlot>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Best-effort: when the user enters a group chat, warm-dial members that are
/// already reachable so the live gossipsub path has a connection. Pure
/// optimization — delivery is guaranteed by the server-inbox fallback, so a
/// dial failure here never blocks sending.
pub fn group_conn_enter(chat_id: &str) {
    if !chat_id.starts_with("grp-") {
        return;
    }
    let now = chatx_core::message::now_ms();
    let needs_connect = {
        let mut m = GROUP_CONNS.lock().unwrap();
        match m.get_mut(chat_id) {
            Some(slot) => {
                slot.last_enter_ms = now;
                false
            }
            None => {
                m.insert(chat_id.to_string(), GroupConnSlot { last_enter_ms: now });
                true
            }
        }
    };
    if !needs_connect {
        return;
    }

    let gid = chat_id.to_string();
    let Some(client) = CLIENT.lock().unwrap().clone() else {
        return;
    };

    std::thread::spawn(move || {
        if let Err(e) = client.connect_to_group(&gid) {
            tracing::warn!("[group] enter {gid} failed: {e}");
        }
    });
}

pub fn touch_chat_activity(chat_id: &str) {
    if chat_id.is_empty() {
        return;
    }
    let now = chatx_core::message::now_ms();
    let mut m = GROUP_CONNS.lock().unwrap();
    match m.get_mut(chat_id) {
        Some(slot) => slot.last_enter_ms = now,
        None => {
            if chat_id.starts_with("grp-") {
                m.insert(chat_id.to_string(), GroupConnSlot { last_enter_ms: now });
            }
        }
    }
}
