use crate::{CLIENT, runtime};

const GROUP_DISCONNECT_GRACE_MS: u64 = 10 * 60 * 1000;
const GROUP_SWEEP_INTERVAL_MS: u64 = 15 * 1000;

#[derive(Clone, Copy)]
struct GroupConnSlot {
    last_enter_ms: u64,
}

static GROUP_CONNS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, GroupConnSlot>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

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

pub fn start_group_conn_sweeper() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let rt = runtime().clone();
    rt.spawn(async move {
        let mut tick =
            tokio::time::interval(std::time::Duration::from_millis(GROUP_SWEEP_INTERVAL_MS));
        loop {
            tick.tick().await;
            let now = chatx_core::message::now_ms();
            let expired: Vec<String> = {
                let m = GROUP_CONNS.lock().unwrap();
                m.iter()
                    .filter(|(_, s)| {
                        now.saturating_sub(s.last_enter_ms) >= GROUP_DISCONNECT_GRACE_MS
                    })
                    .map(|(g, _)| g.clone())
                    .collect()
            };
            if expired.is_empty() {
                continue;
            }
            for gid in &expired {
                GROUP_CONNS.lock().unwrap().remove(gid);
                tracing::warn!("[group] grace expired for {gid} — dropping bookkeeping (conn kept)");
            }
        }
    });
}
