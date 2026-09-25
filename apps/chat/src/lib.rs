use slint::{Image as SlintImage, SharedPixelBuffer, SharedString};
use std::cell::RefCell;
use std::sync::Arc;

use chatx_core::Client;
use chatx_core::signal::HttpDirectory;
use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Side};
use render::Renderer;

pub mod data;
use data::ArcBackend;

slint::include_modules!();

type Directory = HttpDirectory;

thread_local! {
    static RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> = RefCell::new(None);
    static CLIENT: RefCell<Option<Arc<Client<Directory>>>> = RefCell::new(None);
    static POOL: RefCell<Option<Arc<sqlx::SqlitePool>>> = RefCell::new(None);
    static BACKEND: RefCell<Option<ArcBackend>> = RefCell::new(None);
    static RENDERER: RefCell<Option<Renderer>> = RefCell::new(None);
    static CHAT_LOADED: RefCell<std::collections::HashSet<i32>> = RefCell::new(std::collections::HashSet::new());
}

fn runtime() -> Arc<tokio::runtime::Runtime> {
    RUNTIME.with(|slot| {
        if slot.borrow().is_none() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime init failed");
            *slot.borrow_mut() = Some(Arc::new(rt));
        }
        slot.borrow().as_ref().expect("runtime init").clone()
    })
}

fn profile() -> String {
    chatx_core::identity::DeviceIdentity::default_profile()
}

fn keystore_path(profile: &str) -> std::path::PathBuf {
    chatx_core::identity::DeviceIdentity::keystore_path(profile)
}

fn default_server() -> Arc<Directory> {
    Arc::new(Directory::default_server())
}

pub fn main() {
    let ui = MainWindow::new().expect("window init failed");
    let state = ui.global::<AppState>();
    let weak = ui.as_weak();
    state.set_user_id(SharedString::from(""));
    // state.set_is_mobile(cfg!(target_os = "android") || cfg!(target_os = "ios"));
    state.set_is_mobile(true);
    let existing_uid = chatx_core::account::Keystore::load(&keystore_path(&profile()))
        .map(|ks| ks.user_id)
        .ok();
    if let Some(uid) = existing_uid {
        ui.set_auth_message(SharedString::from("please logging in "));
        state.set_user_id(SharedString::from(uid));
    }

    {
        let tabs: Vec<TabState> = (0..4).map(|_| TabState {
            sub_history: slint::ModelRc::new(slint::VecModel::from(Vec::<SubPageEntry>::new())),
            sub_top: -1,
        }).collect();
        let mut nav = state.get_nav_state();
        nav.tabs = slint::ModelRc::new(slint::VecModel::from(tabs));
        state.set_nav_state(nav);
    }
    {
        let w = weak.clone();
        let w2 = weak.clone();
        state.on_push_sub(move |page, payload| {
            if let Some(ui) = w.upgrade() {
                let s = ui.global::<AppState>();
                push_sub_history(&s, SubPageEntry { page, payload });
            }
            if page == SubPageType::ChatRoom {
                load_chat_messages(w2.clone(), payload);
            }
        });
    }
    {
        let w = weak.clone();
        state.on_pop_sub(move || {
            if let Some(ui) = w.upgrade() {
                pop_sub_history(&ui.global::<AppState>());
            }
        });
    }
    {
        let w = weak.clone();
        state.on_clear(move || {
            if let Some(ui) = w.upgrade() {
                clear_sub_history(&ui.global::<AppState>());
            }
        });
    }

    {
        let weak = weak.clone();
        ui.on_login(move |user_id, pass| {
            let uid = user_id.to_string();
            let pass = pass.to_string();
            let profile = profile();
            let dir = default_server();
            let weak = weak.clone();
            if let Some(ui) = weak.upgrade() {
                ui.set_auth_busy(true);
                ui.set_auth_message(SharedString::from("logging in …"));
            }
            let rt = runtime();
            rt.spawn(async move {
                let res = Client::login(&profile, &pass, uid.clone(), dir).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_auth_busy(false);
                    }
                    apply_result(weak, uid, res);
                });
            });
        });
    }

    {
        let weak = weak.clone();
        ui.on_register(move |user_id, pass| {
            let uid = user_id.to_string();
            let pass = pass.to_string();
            let profile = profile();
            let dir = default_server();
            let weak = weak.clone();
            if let Some(ui) = weak.upgrade() {
                ui.set_auth_busy(true);
                ui.set_auth_message(SharedString::from("register …"));
            }
            let rt = runtime();
            rt.spawn(async move {
                let res = Client::bootstrap(&profile, uid.clone(), &pass, dir).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_auth_busy(false);
                    }
                    apply_result(weak, uid, res);
                });
            });
        });
    }

    let w = weak.clone();
    ui.on_logout(move || {
        CLIENT.with(|s| *s.borrow_mut() = None);
        POOL.with(|s| *s.borrow_mut() = None);
        BACKEND.with(|s| *s.borrow_mut() = None);
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            st.set_chats(slint::ModelRc::new(slint::VecModel::from(Vec::<ConversationRow>::new())));
            st.set_contacts(slint::ModelRc::new(slint::VecModel::from(Vec::<ContactRow>::new())));
            st.set_discover(slint::ModelRc::new(slint::VecModel::from(Vec::<DiscoverCard>::new())));
            st.set_data_status(SharedString::new());
            let cs = ui.global::<ChatSession>();
            cs.set_messages(slint::ModelRc::new(slint::VecModel::from(Vec::<MessageData>::new())));
            cs.set_send_status(SharedString::new());
            ui.set_logged_in(false);
        }
    });

    {
        let weak = weak.clone();
        ui.global::<AppState>().on_toggle_like(move |key| {
            toggle_discover_like(weak.clone(), key);
        });
    }

    {
        let weak = weak.clone();
        ui.global::<ChatSession>().on_message_sent(move |key, body| {
            let b = body.as_str().trim().to_string();
            if b.is_empty() {
                return;
            }
            send_chat_message(weak.clone(), key, b);
        });
    }

    ui.run().expect("window run failed");
}

fn push_sub_history(app: &AppState, entry: SubPageEntry) {
    let mut nav = app.get_nav_state();
    let active = nav.active_tab as usize;
    use slint::Model;
    let tabs_model = nav.tabs.clone();
    let mut v: Vec<TabState> = (0..tabs_model.row_count()).filter_map(|i| tabs_model.row_data(i)).collect();
    if active >= v.len() { return; }
    let tab = v.get_mut(active).unwrap();
    let h = tab.sub_history.clone();
    let mut v2: Vec<SubPageEntry> = (0..h.row_count()).filter_map(|i| h.row_data(i)).collect();
    v2.push(entry);
    tab.sub_history = slint::ModelRc::new(slint::VecModel::from(v2));
    tab.sub_top = tab.sub_history.row_count() as i32 - 1;
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(v));
    app.set_nav_state(nav);
}

fn pop_sub_history(app: &AppState) {
    let mut nav = app.get_nav_state();
    let active = nav.active_tab as usize;
    use slint::Model;
    let tabs_model = nav.tabs.clone();
    let mut v: Vec<TabState> = (0..tabs_model.row_count()).filter_map(|i| tabs_model.row_data(i)).collect();
    if active >= v.len() { return; }
    let tab = v.get_mut(active).unwrap();
    let next_top = tab.sub_top - 1;
    if next_top < 0 {
        tab.sub_history = slint::ModelRc::new(slint::VecModel::from(Vec::<SubPageEntry>::new()));
        tab.sub_top = -1;
    } else {
        tab.sub_top = next_top;
    }
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(v));
    app.set_nav_state(nav);
}

fn clear_sub_history(app: &AppState) {
    let tabs: Vec<TabState> = (0..4)
        .map(|_| TabState {
            sub_history: slint::ModelRc::new(slint::VecModel::from(Vec::<SubPageEntry>::new())),
            sub_top: -1,
        })
        .collect();
    let mut nav = app.get_nav_state();
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(tabs));
    app.set_nav_state(nav);
}

fn apply_result(
    weak: slint::Weak<MainWindow>,
    user_id: String,
    res: anyhow::Result<(Client<Directory>, chatx_core::account::Account)>,
) {
    match res {
        Ok((client, _acct)) => {
            let pool = client.store().pool();
            let client = Arc::new(client);
            client.heartbeat();
            CLIENT.with(|s| *s.borrow_mut() = Some(client));

            if let Some(pool) = pool {
                let pool = Arc::new(pool);
                POOL.with(|s| *s.borrow_mut() = Some(pool.clone()));
                let rt = runtime();
                let weak = weak.clone();
                let me = user_id.clone();
                rt.spawn(async move {
                    match data::load(&pool, &me).await {
                        Ok(backend) => {
                            let backend = Arc::new(tokio::sync::RwLock::new(backend));
                            BACKEND.with(|s| *s.borrow_mut() = Some(backend.clone()));
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    publish_to_views(&ui.global::<AppState>(), backend);
                                }
                            });
                        }
                        Err(err) => {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.global::<AppState>()
                                        .set_data_status(SharedString::from(format!("load failed: {err}")));
                                }
                            });
                        }
                    }
                });
            }
        }
        Err(err) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_auth_message(SharedString::from(err.to_string()));
            }
            return;
        }
    }

    if let Some(ui) = weak.upgrade() {
        ui.set_auth_message(SharedString::from(format!("logged in:{user_id}")));
        ui.global::<AppState>().set_user_id(SharedString::from(user_id));
        ui.set_logged_in(true);
    }
}

/// Push the in-memory backend rows into the slint view models (memory -> UI).
fn publish_to_views(state: &AppState, backend: ArcBackend) {
    let snapshot = backend.blocking_read().clone();

    let chats: Vec<ConversationRow> = snapshot
        .chats
        .iter()
        .map(|r| ConversationRow {
            chat_id: r.key,
            title: SharedString::from(r.title.clone()),
            preview: SharedString::from(r.preview.clone()),
            is_group: r.is_group,
            time_label: SharedString::from(fmt_time(r.time_ms)),
        })
        .collect();
    state.set_chats(slint::ModelRc::new(slint::VecModel::from(chats)));

    let contacts: Vec<ContactRow> = snapshot
        .contacts
        .iter()
        .map(|r| ContactRow {
            id: r.key,
            peer_id: SharedString::from(r.peer_id.clone()),
            name: SharedString::from(r.name.clone()),
        })
        .collect();
    state.set_contacts(slint::ModelRc::new(slint::VecModel::from(contacts)));

    let discover: Vec<DiscoverCard> = snapshot
        .discover
        .iter()
        .map(|r| DiscoverCard {
            id: r.key,
            title: SharedString::from(r.title.clone()),
            user: SharedString::from(r.author.clone()),
            likes: r.likes as i32,
            liked: r.liked,
        })
        .collect();
    state.set_discover(slint::ModelRc::new(slint::VecModel::from(discover)));

    state.set_data_status(SharedString::new());
}

/// Ensure the bubble renderer exists (fonts + emoji atlas loaded once).
fn renderer() -> Renderer {
    let mut r = Renderer::new(render::theme::Theme::default());
    for path in [
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "C:/Windows/Fonts/msyh.ttc",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            if let Some(id) = r.fonts.add_font(bytes) {
                let chain = r.fonts.ltr_chain().to_vec();
                r.fonts.set_ltr_chain(&{
                    let mut v = chain;
                    v.push(id);
                    v
                });
                let rchain = r.fonts.rtl_chain().to_vec();
                r.fonts.set_rtl_chain(&{
                    let mut v = rchain;
                    v.push(id);
                    v
                });
            }
        }
    }
    let px = r.theme.font_size.max(16.0);
    if let Ok(bytes) = std::fs::read("/System/Library/Fonts/Apple Color Emoji.ttc") {
        r.emoji.add_color_font_bytes(&bytes, 0, &['😀','😂','😅','😉','😊','😍','😘','😜','😎','😢','😭','😡','👍','👎','🙏','👏','🎉','🔥','💯','🚀'], px);
    }
    r
}

fn ensure_renderer() -> bool {
    let exists = RENDERER.with(|s| s.borrow().is_some());
    if !exists {
        RENDERER.with(|s| *s.borrow_mut() = Some(renderer()));
    }
    true
}

/// Render one stored text message into a slint [MessageData] row.
fn render_one_message(
    r: &mut Renderer,
    id: u64,
    sender: &str,
    text: &str,
    is_self: bool,
    time: &str,
    available_w: u32,
    scale: f32,
) -> MessageData {
    let segs = parse_text(text);
    let bubble = Bubble {
        segments: &segs,
        sender,
        time,
        side: if is_self { Side::SelfSide } else { Side::Other },
        group: GroupPos::Single,
        avatar: None,
    };
    let out = r.render(id, &bubble, available_w, scale).clone();
    let rgba = out.to_straight_rgba();
    let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(out.width, out.height);
    buf.make_mut_bytes().copy_from_slice(&rgba);
    MessageData {
        bubble: SlintImage::from_rgba8(buf),
        width: (out.width as f64 / scale as f64) as f32,
        height: (out.height as f64 / scale as f64) as f32,
        is_self,
        text: SharedString::from(text.to_string()),
        selected: false,
    }
}

/// Load a conversation's messages from SQLite into the ChatSession global (memory -> UI).
fn load_chat_messages(ui_weak: slint::Weak<MainWindow>, chat_key: i32) {
    let client: Option<Arc<Client<HttpDirectory>>> = CLIENT.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(client), Some(pool), Some(backend)) = (client, pool, backend) else {
        return;
    };
    let chat_id = {
        let g = backend.blocking_read();
        g.chat_id_for(chat_key).map(|s| s.to_string())
    };
    let Some(chat_id) = chat_id else { return };
    let _ = pool;
    let me_peer = client.peer_base58().to_string();
    let me = client.user_id().to_string();
    let title = {
        let g = backend.blocking_read();
        g.chats.iter().find(|r| r.key == chat_key).map(|r| r.title.clone()).unwrap_or_default()
    };
    let rt = runtime();
    let ui_weak2 = ui_weak.clone();
    rt.spawn(async move {
        let rows = match client.store().load(&chat_id, 200, 0).await {
            Ok(rows) => rows,
            Err(e) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.global::<ChatSession>()
                            .set_send_status(SharedString::from(format!("加载消息失败: {e}")));
                    }
                });
                return;
            }
        };
        ensure_renderer();
        let mut rows = rows;
        rows.reverse();
        let rendered: Vec<MessageData> = RENDERER.with(|slot| {
            let mut g = slot.borrow_mut();
            let Some(r) = g.as_mut() else { return Vec::new() };
            rows.iter().enumerate().map(|(i, m)| {
                let is_self = m.sender == me_peer || m.sender == me;
                let time = time_label(m.t as i64);
                let sender = if is_self { "我" } else { &title };
                render_one_message(r, i as u64, sender, &m.text, is_self, &time, 380, 2.0)
            }).collect()
        });
        if let Some(ui) = ui_weak2.upgrade() {
            let cs = ui.global::<ChatSession>();
            cs.set_messages(slint::ModelRc::new(slint::VecModel::from(rendered)));
            cs.set_send_status(SharedString::new());
        }
    });
}

fn time_label(ms: i64) -> String {
    if ms <= 0 {
        return "刚刚".into();
    }
    let secs = (ms / 1000) as i64;
    let day_start = (secs / 86_400) * 86_400;
    let h = ((secs - day_start) / 3600) % 24;
    let m = ((secs - day_start) % 3600) / 60;
    format!("{h:02}:{m:02}")
}

/// Sent callback from ChatView: resolve peer, send via P2P, then re-render the chat.
fn send_chat_message(weak: slint::Weak<MainWindow>, key: i32, body: String) {
    let client = CLIENT.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(client), Some(backend)) = (client, backend) else {
        return;
    };
    let chat_id = {
        let g = backend.blocking_read();
        g.chat_id_for(key).map(|s| s.to_string())
    };
    let Some(chat_id) = chat_id else { return };
    if let Some(ui) = weak.upgrade() {
        ui.global::<ChatSession>().set_send_status(SharedString::from("发送中…"));
    }
    let candidates: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        if let Some(p) = client.other_peer_of(&chat_id) {
            v.push(p);
        }
        for part in chat_id.split('|') {
            if !part.is_empty() && part != client.peer_base58() && !v.iter().any(|x| x == part) {
                v.push(part.to_string());
            }
        }
        if v.is_empty() {
            v.push(chat_id.clone());
        }
        v
    };
    let rt = runtime();
    let weak2 = weak.clone();
    rt.spawn(async move {
        let mut last_err = String::new();
        let mut sent = false;
        'outer: for cand in &candidates {
            let ur = match client.resolve_user(cand) {
                Ok(ur) if ur.device.peer_id != client.peer_base58() => ur,
                _ => continue,
            };
            match client.send_dm(&ur.device.peer_id, &body).await {
                Ok(_) => {
                    sent = true;
                    break 'outer;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        if !sent {
            let msg = if last_err.trim().is_empty() {
                "对端不在线".to_string()
            } else {
                last_err
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ChatSession>()
                        .set_send_status(SharedString::from(format!("发送失败: {msg}")));
                }
            });
            return;
        }
        {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ChatSession>().set_send_status(SharedString::new());
                }
            });
            load_chat_messages(weak2, key);
        }
    });
}

/// Compact, human time label for the chat list (relative to now).
fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff = now.saturating_sub(ms);
    if diff < 60_000 {
        "刚刚".into()
    } else if diff < 3_600_000 {
        format!("{}分钟前", diff / 60_000)
    } else if diff < 86_400_000 {
        format!("{}小时前", diff / 3_600_000)
    } else if diff < 86_400_000 * 365 {
        format!("{}天前", diff / 86_400_000)
    } else {
        format!("{}年前", diff / 86_400_000 / 365)
    }
}

/// Toggle a discovery like: memory first, then async persist to SQLite.
fn toggle_discover_like(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let (Some(backend), Some(pool)) = (backend, pool) else {
        return;
    };
    let (post_id, want_liked, me) = {
        let mut g = backend.blocking_write();
        let (liked, _likes) = g.toggle_like(key);
        (g.post_id_for(key).map(|s| s.to_string()), liked, g.me.clone())
    };
    // memory -> UI
    if let Some(ui) = weak.upgrade() {
        publish_to_views(&ui.global::<AppState>(), backend.clone());
    }
    // memory -> sqlite (async, fire-and-forget)
    if let Some(post_id) = post_id {
        let rt = runtime();
        rt.spawn(async move {
            let _ = data::persist_like_toggle(&pool, &me, &post_id, want_liked).await;
        });
    }
}
