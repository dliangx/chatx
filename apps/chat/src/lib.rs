use slint::{Image as SlintImage, SharedPixelBuffer, SharedString};
use std::cell::RefCell;
use std::sync::Arc;

use chatx_core::Client;
use chatx_core::group::MemberInfo;
use chatx_core::signal::{DirectoryClient, HttpDirectory};
use chatx_core::store::dm_chat_id;
use render::bubble::parse::parse_text;
use render::bubble::{Bubble, GroupPos, Side};
use render::Renderer;

pub mod data;
use data::ArcBackend;
pub mod call;

// `camera::Camera` exists on every target (Apple: AVFoundation impl, others:
// no-op fallback whose `start()` reports "unsupported"), so import it
// unconditionally. The *usage* below is still gated to Apple.
use camera::Camera;
use screen::Screen;

slint::include_modules!();

type Directory = HttpDirectory;

thread_local! {
    static RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> = RefCell::new(None);
    static CLIENT: RefCell<Option<Arc<Client<Directory>>>> = RefCell::new(None);
    static POOL: RefCell<Option<Arc<sqlx::SqlitePool>>> = RefCell::new(None);
    static BACKEND: RefCell<Option<ArcBackend>> = RefCell::new(None);
    static CHAT_LOADED: RefCell<std::collections::HashSet<i32>> = RefCell::new(std::collections::HashSet::new());
    static GROUP_PICKED: RefCell<Vec<i32>> = RefCell::new(Vec::new());
    static PUMP_STARTED: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static CAMERA: RefCell<Option<Camera>> = RefCell::new(None);
    static SCREEN: RefCell<Option<Screen>> = RefCell::new(None);
}

/// Shared renderer, initialized lazily (font parsing is expensive) and shared
/// across worker threads via a mutex so it is only built once.
static RENDERER: std::sync::OnceLock<std::sync::Mutex<Renderer>> = std::sync::OnceLock::new();

/// Monotonic id for profile loads (each request gets a fresh id).
static PROFILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Id of the most-recent profile load; stale ones are dropped before publishing.
static PROFILE_LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Monotonic id for post-detail loads (each request gets a fresh id).
static POST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Id of the most-recent post-detail load; stale ones are dropped before publishing.
static POST_LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

fn autologin_path() -> std::path::PathBuf {
    keystore_path(&profile()).with_file_name("autologin")
}

/// Persist the passphrase in a local file so the app can auto-login next time.
fn save_passphrase(_user_id: &str, pass: &str) {
    let _ = std::fs::write(autologin_path(), pass.as_bytes());
}

fn load_passphrase(_user_id: &str) -> Option<String> {
    std::fs::read_to_string(autologin_path()).ok()
}

fn clear_passphrase(_user_id: &str) {
    let _ = std::fs::remove_file(autologin_path());
}

pub fn run_app() {
    // Mobile only: install the bridge consumers. iOS and Android both use
    // the same bridge crate (jni.rs on Android, ffi.rs on iOS); desktop
    // uses the native `screen` and `camera` crates directly and has no
    // bridge at all, so this cfg is a no-op there.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    install_bridge_sinks();

    let ui = MainWindow::new().expect("window init failed");
    let state = ui.global::<AppState>();
    let weak = ui.as_weak();
    state.set_user_id(SharedString::from(""));
    state.set_is_mobile(cfg!(target_os = "android") || cfg!(target_os = "ios"));
    // state.set_is_mobile(true);
    let existing_uid = chatx_core::account::Keystore::load(&keystore_path(&profile()))
        .map(|ks| ks.user_id)
        .ok();
    if let Some(uid) = existing_uid {
        ui.set_auth_message(SharedString::from("please logging in "));
        state.set_user_id(SharedString::from(uid.clone()));

        if let Some(pass) = load_passphrase(&uid) {
            ui.set_logged_in(true);
            let profile = profile();
            let dir = default_server();
            let weak = weak.clone();
            let rt = runtime();
            rt.spawn(async move {
                let res = Client::login(&profile, &pass, uid.clone(), dir).await;
                if res.is_err() {
                    clear_passphrase(&uid);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    apply_result(weak, uid, res);
                });
            });
        }
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
            if page == SubPageType::ChatRoom {
                load_chat_messages(w2.clone(), payload);
            }
            if page == SubPageType::MyNote {
                refresh_my_notes(w2.clone());
            }
            if let Some(ui) = w.upgrade() {
                let s = ui.global::<AppState>();
                push_sub_history(&s, SubPageEntry { page, payload });
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
                if res.is_ok() {
                    save_passphrase(&uid, &pass);
                }
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
                if res.is_ok() {
                    save_passphrase(&uid, &pass);
                }
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
    ui.global::<AppState>().on_logout(move || {
        CLIENT.with(|s| *s.borrow_mut() = None);
        POOL.with(|s| *s.borrow_mut() = None);
        BACKEND.with(|s| *s.borrow_mut() = None);
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            let uid = st.get_user_id().to_string();
            if !uid.is_empty() {
                clear_passphrase(&uid);
            }
            clear_sub_history(&st);
            st.set_chats(slint::ModelRc::new(slint::VecModel::from(Vec::<ConversationRow>::new())));
            st.set_contacts(slint::ModelRc::new(slint::VecModel::from(Vec::<ContactRow>::new())));
            st.set_contact_letters(slint::ModelRc::new(slint::VecModel::from(Vec::<LetterEntry>::new())));
            st.set_discover(slint::ModelRc::new(slint::VecModel::from(Vec::<DiscoverCard>::new())));
            st.set_my_collect(slint::ModelRc::new(slint::VecModel::from(Vec::<DiscoverCard>::new())));
            st.set_my_share(slint::ModelRc::new(slint::VecModel::from(Vec::<DiscoverCard>::new())));
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

    // ---- My note: save (from AddNoteView) / delete ----
    {
        let weak = weak.clone();
        ui.global::<NoteState>().on_note_saved(move |text| {
            save_my_note(weak.clone(), text.to_string());
        });
    }
    {
        let weak = weak.clone();
        ui.global::<AppState>().on_delete_note(move |id| {
            delete_my_note(weak.clone(), id);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<AppState>().on_notes_layout(move |w| {
            relayout_notes(weak.clone(), w);
        });
    }

    {
        ui.global::<AppState>().on_pick_group_member(move |key| {
            GROUP_PICKED.with(|s| {
                if !s.borrow().contains(&key) {
                    s.borrow_mut().push(key);
                }
            });
        });
    }
    {
        ui.global::<AppState>().on_unpick_group_member(move |key| {
            GROUP_PICKED.with(|s| s.borrow_mut().retain(|k| *k != key));
        });
    }
    {
        let w = weak.clone();
        ui.global::<AppState>().on_create_group(move || {
            let picked = GROUP_PICKED.with(|s| s.borrow().clone());
            if create_group_flow(w.clone(), picked) {
                GROUP_PICKED.with(|s| s.borrow_mut().clear());
            }
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_show_qr_code(move || {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let mut nav = st.get_nav_state();
                nav.global_overlay = GlobalOverlayType::QrCode;
                st.set_nav_state(nav);
            }
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_close_qr_code(move || {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let mut nav = st.get_nav_state();
                nav.global_overlay = GlobalOverlayType::None;
                st.set_nav_state(nav);
            }
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_search_users(move |q| {
            search_users(w.clone(), q.to_string());
        });
    }
    {
        let w = weak.clone();
        ui.global::<AppState>().on_add_contact(move |username| {
            add_contact(w.clone(), username.to_string());
        });
    }

    // ---- Contact profile actions ----
    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_request_load(move |key| {
            load_profile_data(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_open_conversation(move |key| {
            open_conversation_with_contact(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_start_video_call(move || {
            show_call_overlay(weak.clone(), GlobalOverlayType::VideoCall);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_start_screen_share(move || {
            begin_screen_share(weak.clone());
        });
    }
    {
        let weak = weak.clone();
        ui.global::<AppState>().on_stop_screen_share(move || {
            end_screen_share(weak.clone());
        });
    }

    // ---- Realtime voice call (M7) ----
    {
        let weak = weak.clone();
        ui.global::<CallState>().on_start_call(move |key| {
            begin_voice_call(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<CallState>().on_end_call(move || {
            end_voice_call(weak.clone());
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

    // ---- Discover post detail: load / like / comment ----
    {
        let weak = weak.clone();
        ui.global::<PostDetailState>().on_request_load(move |key| {
            load_post_detail(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<PostDetailState>().on_toggle_like(move |key| {
            toggle_detail_like(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<PostDetailState>().on_add_comment(move |key, body| {
            let b = body.as_str().trim().to_string();
            if b.is_empty() {
                return;
            }
            add_post_comment(weak.clone(), key, b);
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_scanner_start(move || {
            let Some(ui) = w.upgrade() else {
                return;
            };
            let state = ui.global::<AppState>();
            state.set_scan_found(false);
            state.set_scan_result(SharedString::new());
            state.set_scan_status(SharedString::from("摄像头已启动，请对准二维码…"));
            start_camera_scanner(w.clone(), state);
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_scanner_stop(move || {
            CAMERA.with(|slot| {
                if let Some(cam) = slot.borrow_mut().as_mut() {
                    cam.stop();
                }
            });
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_scan_status(SharedString::new());
            }
        });
    }

    {
        let rt = runtime();
        rt.spawn(async move {
            ensure_renderer();
        });
    }

    ui.run().expect("window run failed");
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: slint::android::AndroidApp) {
    slint::android::init(app).unwrap();
    run_app();
}

#[allow(dead_code)]
fn install_bridge_sinks() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FRAME_N: AtomicU64 = AtomicU64::new(0);
    static AUDIO_N: AtomicU64 = AtomicU64::new(0);

    bridge::set_camera_consumer(Box::new(move |bytes, w, h, fmt| {
        let n = FRAME_N.fetch_add(1, Ordering::Relaxed);
        if n % 100 == 0 {
            eprintln!("[bridge] camera #{} {}x{} {}B fmt={}", n, w, h, bytes.len(), fmt);
        }
    }));
    bridge::set_audio_consumer(Box::new(move |bytes, rate, ch| {
        let n = AUDIO_N.fetch_add(1, Ordering::Relaxed);
        if n % 100 == 0 {
            eprintln!("[bridge] audio #{} {}B @{}Hz/{}ch", n, bytes.len(), rate, ch);
        }
    }));
    bridge::set_screen_consumer(Box::new(move |bytes, w, h, fmt| {
        on_screen_frame(bytes, w, h, fmt);
    }));
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
    let keep = if tab.sub_top < 0 { 0 } else { tab.sub_top as usize + 1 };
    let mut v2: Vec<SubPageEntry> = (0..h.row_count()).filter_map(|i| h.row_data(i)).collect();
    v2.truncate(keep);
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
    let mut v: Vec<TabState> = (0..nav.tabs.row_count()).filter_map(|i| nav.tabs.row_data(i)).collect();
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
    nav.active_tab = 0;
    nav.global_overlay = GlobalOverlayType::None;
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(tabs));
    app.set_nav_state(nav);
}

/// Open the camera and wire decoded QR payloads into the scan UI.
///
/// `camera::Camera` exposes the same API on every target (Apple AVFoundation,
/// Windows/Linux nokhwa, Android bridge+JNI), so this helper is platform-
/// agnostic: start it, install a sink that hops back to the UI thread, and be
/// done. Any platform whose `start()` reports "unsupported" surfaces the error
/// in `scan-status`.
    fn start_camera_scanner(weak: slint::Weak<MainWindow>, state: AppState) {
    CAMERA.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Camera::new());
        }
        let mut guard = slot.borrow_mut();
        let cam = guard.as_mut().expect("camera inited");
        if cam.is_running() {
            return;
        }
        match cam.start() {
            Ok(_) => {
                let wsink = weak.clone();
                cam.set_sink(move |text: String| {
                    let wk = wsink.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = wk.upgrade() {
                            let st = ui.global::<AppState>();
                            st.set_scan_found(true);
                            st.set_scan_result(SharedString::from(text));
                        }
                    });
                });
            }
            Err(err) => {
                state.set_scan_status(SharedString::from(format!("无法启动摄像头: {err}")));
            }
        }
    });
}

/// Render a QR code for `text` as an RGBA slint image (black modules on white).
fn qr_image(text: &str) -> slint::Image {
    use qrcode::{Color, QrCode};
    let code = match QrCode::new(text.as_bytes()) {
        Ok(c) => c,
        Err(_) => return slint::Image::default(),
    };
    let n = code.width();
    let scale = 8usize;
    let size = n * scale;
    let colors = code.to_colors();
    let mut rgba = vec![255u8; size * size * 4];
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let px = x * scale + dx;
                        let py = y * scale + dy;
                        let i = (py * size + px) * 4;
                        rgba[i] = 0;
                        rgba[i + 1] = 0;
                        rgba[i + 2] = 0;
                        rgba[i + 3] = 255;
                    }
                }
            }
        }
    }
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(size as u32, size as u32);
    buf.make_mut_bytes().copy_from_slice(&rgba);
    slint::Image::from_rgba8(buf)
}

fn apply_result(
    weak: slint::Weak<MainWindow>,
    user_id: String,
    res: anyhow::Result<(Client<Directory>, chatx_core::account::Account)>,
) {
    match res {
        Ok((client, _acct)) => {
            let pool = client.store().pool();
            let peer = client.peer_base58().to_string();
            let client = Arc::new(client);
            client.heartbeat();
            CLIENT.with(|s| *s.borrow_mut() = Some(client));

            start_inbound_pump(weak.clone());

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
                            let nickname = backend.read().await.my_nickname.clone();
                            let peer = peer.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                BACKEND.with(|s| *s.borrow_mut() = Some(backend.clone()));
                                if let Some(ui) = weak.upgrade() {
                                    let state = ui.global::<AppState>();
                                    state.set_peer_id(SharedString::from(peer.clone()));
                                    state.set_qr_image(qr_image(&peer));
                                    state.set_nickname(SharedString::from(nickname));
                                    publish_to_views(&state, backend);
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
                ui.set_logged_in(false);
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

/// Spawn a long-lived task that receives inbound events (DM, group key
/// distribution, group messages, and realtime audio) and refreshes the
/// affected conversations in the UI. Started once per login.
fn start_inbound_pump(weak: slint::Weak<MainWindow>) {
    if PUMP_STARTED.get() {
        return;
    }
    PUMP_STARTED.set(true);
    let weak = weak.clone();
    let rt = runtime();
    rt.spawn(async move {
        loop {
            let client = match CLIENT.with(|s| s.borrow().clone()) {
                Some(c) => c,
                None => break,
            };
            let Some(evt) = client.next_event().await else {
                break;
            };
            // ── Realtime audio: route directly to the speaker queue. ──────
            if let chatx_core::swarm::ChatEvent::Audio { data, rate, ch, .. } = &evt {
                let data = data.clone();
                let (r2, c2) = (*rate, *ch);
                let _ = slint::invoke_from_event_loop(move || {
                    call::play_incoming(data, r2, c2);
                });
                continue;
            }
            let touched = client.process_event(evt).await;
            if let Some(touched) = touched {
                refresh_inbound_chat(weak.clone(), touched).await;
            }
        }
    });
}

/// After an inbound message touched a conversation, patch its chat-list row and
/// reload the message list if that conversation is the one currently shown.
async fn refresh_inbound_chat(weak: slint::Weak<MainWindow>, chat_id: String) {
    let pool = POOL.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(pool), Some(backend)) = (pool, backend) else {
        return;
    };
    {
        let mut b: data::DataBackend = backend.read().await.clone();
        let _ = data::refresh_chat_row(&pool, &mut b, &chat_id).await;
        *backend.write().await = b;
    }
    let is_open_key = {
        backend.read().await.chats.iter().find(|r| r.chat_id == chat_id).map(|r| r.key)
    };
    if let Some(ui) = weak.upgrade() {
        let state = ui.global::<AppState>();
        publish_to_views(&state, backend.clone());
        if let Some(key) = is_open_key {
            use slint::Model;
            let nav = state.get_nav_state();
            let tabs = nav.tabs.clone();
            if let Some(tab) = tabs.row_data(nav.active_tab as usize) {
                let top = tab.sub_top;
                if top >= 0 {
                    let hist = tab.sub_history.clone();
                    if let Some(entry) = hist.row_data(top as usize) {
                        if entry.page == SubPageType::ChatRoom && entry.payload == key {
                            load_chat_messages(ui.as_weak(), key);
                        }
                    }
                }
            }
        }
    }
}

/// Push the in-memory backend rows into the slint view models (memory -> UI).
fn publish_to_views(state: &AppState, backend: ArcBackend) {
    let mut snapshot = backend.blocking_read().clone();

    // WeChat-style order: A..Z by pinyin initial, '#' catch-all last,
    // alphabetical within a section. Computed here so every publish path
    // (initial load, appended friend) keeps the right-side index in order.
    let rank = |c: char| if c == '#' { '[' } else { c };
    snapshot
        .contacts
        .sort_by(|a, b| rank(data::contact_letter(&a.name))
            .cmp(&rank(data::contact_letter(&b.name)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

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

    // Letter of each contact (pinyin initial) — the backend already sorted the
    // list by this, so consecutive same-letter entries form a WeChat-style section.
    let contact_letters: Vec<char> = snapshot
        .contacts
        .iter()
        .map(|r| data::contact_letter(&r.name))
        .collect();

    let contacts: Vec<ContactRow> = snapshot
        .contacts
        .iter()
        .map(|r| ContactRow {
            id: r.key,
            image: slint::Image::load_from_path(std::path::Path::new(&r.image)).unwrap_or_default(),
            peer_id: SharedString::from(r.peer_id.clone()),
            name: SharedString::from(r.name.clone()),
        })
        .collect();

    // A–Z index bar entries: one per distinct letter, in list order (A..Z, '#'
    // last), targeting the index of the first contact of that letter.
    let mut letter_entries: Vec<(char, i32)> = Vec::new();
    for (i, &ltr) in contact_letters.iter().enumerate() {
        if letter_entries.last().map(|(c, _)| *c).map_or(true, |c| c != ltr) {
            letter_entries.push((ltr, i as i32));
        }
    }
    let letters_model: Vec<LetterEntry> = letter_entries
        .into_iter()
        .map(|(ltr, target)| LetterEntry {
            letter: SharedString::from(ltr.to_string()),
            target,
        })
        .collect();

    state.set_contacts(slint::ModelRc::new(slint::VecModel::from(contacts)));
    state.set_contact_letters(slint::ModelRc::new(slint::VecModel::from(letters_model)));

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

    let my_collect: Vec<DiscoverCard> = snapshot
        .my_collect
        .iter()
        .map(|r| DiscoverCard {
            id: r.key,
            title: SharedString::from(r.title.clone()),
            user: SharedString::from(r.author.clone()),
            likes: r.likes as i32,
            liked: r.liked,
        })
        .collect();
    state.set_my_collect(slint::ModelRc::new(slint::VecModel::from(my_collect)));

    let my_share: Vec<DiscoverCard> = snapshot
        .my_share
        .iter()
        .map(|r| DiscoverCard {
            id: r.key,
            title: SharedString::from(r.title.clone()),
            user: SharedString::from(r.author.clone()),
            likes: r.likes as i32,
            liked: r.liked,
        })
        .collect();
    state.set_my_share(slint::ModelRc::new(slint::VecModel::from(my_share)));

    state.set_data_status(SharedString::new());
}

/// Bundled fonts (SIL OFL / Apache-2.0), embedded at compile time.
const FONT_LATIN: &[u8] = include_bytes!("../fonts/NotoSans.ttf");
const FONT_CJK: &[u8] = include_bytes!("../fonts/NotoSansSC.ttf");
const FONT_EMOJI: &[u8] = include_bytes!("../fonts/NotoColorEmoji.ttf");

/// Ensure the bubble renderer exists (fonts + emoji atlas loaded once).
fn renderer() -> Renderer {
    let mut r = Renderer::new(render::theme::Theme::default());
    for bytes in [FONT_LATIN, FONT_CJK] {
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
    let px = r.theme.font_size.max(16.0);
    r.emoji.add_color_font_bytes(FONT_EMOJI, 0, &['😀','😂','😅','😉','😊','😍','😘','😜','😎','😢','😭','😡','👍','👎','🙏','👏','🎉','🔥','💯','🚀'], px);
    r
}

fn ensure_renderer() {
    let _ = RENDERER.get_or_init(|| std::sync::Mutex::new(renderer()));
}

/// Intermediate render result produced on a background thread.
/// The slint image type is not `Send`, so we pass raw bytes across threads.
struct RenderedMsg {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    is_self: bool,
    text: String,
}

/// Render one message into raw RGBA bytes (background thread; avoids building
/// the non-`Send` slint image off the UI thread).
fn render_msg(
    r: &mut Renderer,
    id: u64,
    sender: &str,
    text: &str,
    is_self: bool,
    time: &str,
    available_w: u32,
    scale: f32,
) -> RenderedMsg {
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
    RenderedMsg {
        rgba: out.to_straight_rgba(),
        width: out.width,
        height: out.height,
        is_self,
        text: text.to_string(),
    }
}

/// Build a slint [MessageData] from raw bytes on the UI thread.
fn to_message_data(m: RenderedMsg, scale: f32) -> MessageData {
    let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(m.width, m.height);
    buf.make_mut_bytes().copy_from_slice(&m.rgba);
    MessageData {
        bubble: SlintImage::from_rgba8(buf),
        width: (m.width as f64 / scale as f64) as f32,
        height: (m.height as f64 / scale as f64) as f32,
        is_self: m.is_self,
        text: SharedString::from(m.text),
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
        // Render on a background thread: font parsing is the slow part and must
        // not block the UI; we only ship raw bytes across to the UI thread.
        ensure_renderer();
        let mut rows = rows;
        rows.reverse();
        let rendered: Vec<RenderedMsg> = {
            let mut r = RENDERER.get().unwrap().lock().unwrap();
            rows.iter().map(|m| {
                let is_self = m.sender == me_peer || m.sender == me;
                let time = time_label(m.t as i64);
                let sender = if is_self { "我" } else { &title };
                // Key the texture cache by rendered content (not the row id), so
                // editing a message in the DB invalidates the stale bubble.
                let mut h = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&m.text, &mut h);
                std::hash::Hash::hash(sender, &mut h);
                std::hash::Hash::hash(&is_self, &mut h);
                std::hash::Hash::hash(&time, &mut h);
                render_msg(&mut r, std::hash::Hasher::finish(&h), sender, &m.text, is_self, &time, 380, 2.0)
            }).collect()
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak2.upgrade() {
                let cs = ui.global::<ChatSession>();
                let messages: Vec<MessageData> = rendered
                    .into_iter()
                    .map(|m| to_message_data(m, 2.0))
                    .collect();
                cs.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
                cs.set_send_status(SharedString::new());
            }
        });
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
    let is_group = {
        backend.blocking_read().chats.iter().find(|r| r.key == key).map(|r| r.is_group).unwrap_or(false)
    };

    if is_group {
        if let Some(ui) = weak.upgrade() {
            ui.global::<ChatSession>().set_send_status(SharedString::from("发送中…"));
        }
        let rt = runtime();
        let weak2 = weak.clone();
        rt.spawn(async move {
            match client.send_group_message(&chat_id, &body).await {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<ChatSession>().set_send_status(SharedString::new());
                        }
                    });
                    load_chat_messages(weak2, key);
                }
                Err(e) => {
                    let msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<ChatSession>()
                                .set_send_status(SharedString::from(format!("发送失败: {msg}")));
                        }
                    });
                }
            }
        });
        return;
    }

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
        (g.post_id_for(key), liked, g.me.clone())
    };
    // memory -> UI
    if let Some(ui) = weak.upgrade() {
        publish_to_views(&ui.global::<AppState>(), backend.clone());
    }
    // memory -> sqlite (async, fire-and-forget)
    if let Some(post_id) = post_id {
        let rt = runtime();
        rt.spawn(async move {
            let _ = data::persist_like_toggle(&pool, &me, post_id, want_liked).await;
        });
    }
}

/// Compute masonry positions for the note cards at the given logical container
/// width. Cards flow bottom-up into the currently-shortest column, so column
/// heights differ — the classic "waterfall" look.
///
/// Returns (rows, content_height). `rows` already carry their absolute
/// x / y / width / height so the Slint layer only has to paint them.
fn layout_my_notes(
    notes: &[data::NoteRowData],
    container_w: i32,
    is_mobile: bool,
) -> (Vec<NoteRow>, i32) {
    const GAP: i32 = 8;
    const MIN_CARD_H: i32 = 132;
    const FONT_SIZE: i32 = 15;
    const LINE_H: i32 = 18;
    const PAD_X: i32 = 28;
    const PAD_TOP: i32 = 12;
    const LABEL_H: i32 = 24;
    const PAD_BOTTOM: i32 = 12;

    let n_cols = if is_mobile { 1 } else { 2 } as usize;
    let card_w = if n_cols > 1 {
        ((container_w - (n_cols - 1) as i32 * GAP) / n_cols as i32).max(1)
    } else {
        container_w
    };
    let inner_w = (card_w - PAD_X).max(0);

    // col_top[j] = the y where the next card in column j should be placed.
    let mut col_top = vec![GAP; n_cols as usize];
    let mut rows: Vec<NoteRow> = Vec::with_capacity(notes.len());
    let mut content_h = 0i32;

    // Effective units: 1.0 for CJK / full-width chars, 0.6 for Latin / digits
    // (average Latin glyph ≈ 0.6em). Divide by units_per_line to get lines.
    let units_per_line = inner_w as f64 / FONT_SIZE as f64;

    for n in notes {
        let mut units: f64 = 0.0;
        for ch in n.content.chars() {
            let full = (ch as u32) >= 0x1100
                && ((ch as u32 <= 0x11FF)
                    || (0x2E80..=0x9FFF).contains(&(ch as u32))
                    || (0xAC00..=0xD7FF).contains(&(ch as u32))
                    || (0xFF00..=0xFFEF).contains(&(ch as u32)));
            units += if full { 1.0 } else { 0.6 };
        }
        let lines = if units_per_line > 0.0 {
            (units / units_per_line).ceil() as i32
        } else {
            1
        };
        let card_h = (PAD_TOP + lines * LINE_H + LABEL_H + PAD_BOTTOM).max(MIN_CARD_H);

        // place into the currently-shortest column
        let mut c = 0;
        for j in 1..col_top.len() {
            if col_top[j] < col_top[c] {
                c = j;
            }
        }
        let y = col_top[c];
        let x = (c as i32) * (card_w + GAP);
        col_top[c] = y + card_h + GAP;
        content_h = (y + card_h).max(content_h);

        rows.push(NoteRow {
            id: n.key,
            content: SharedString::from(n.content.clone()),
            created_label: SharedString::from(fmt_time(n.created_at)),
            card_x: x,
            card_y: y,
            card_w,
            card_h,
        });
    }
    (rows, content_h)
}

/// Publish the backend's notes into the Slint `my_notes` view model, laying
/// them out as a masonry grid. Uses the last known card-area width from
/// `AppState.my-notes-w` (pushed by MyNoteView on resize); falls back to the
/// main window width if that hasn't been set yet.
fn publish_my_notes(state: &AppState, backend: &ArcBackend) {
    let notes = backend.blocking_read().notes.clone();
    let w = state.get_my_notes_w().max(1);
    let (rows, content_h) = layout_my_notes(&notes, w, state.get_is_mobile());
    state.set_my_notes(slint::ModelRc::new(slint::VecModel::from(rows)));
    state.set_my_notes_content_h(content_h);
}

/// MyNoteView reports its actual card-area width (on first render and on
/// resize) → re-layout the cards at that width.
fn relayout_notes(weak: slint::Weak<MainWindow>, w: i32) {
    let backend = BACKEND.with(|s| s.borrow().clone());
    let Some(backend) = backend else { return; };
    if let Some(ui) = weak.upgrade() {
        let state = ui.global::<AppState>();
        state.set_my_notes_w(w.max(1));
        publish_my_notes(&state, &backend);
    }
}

/// Load a user's notes from SQLite into the backend, then publish to the UI.
/// DB work runs on the tokio runtime; the backend mutation + publish happen on
/// the event-loop thread (where blocking is allowed).
fn refresh_my_notes(weak: slint::Weak<MainWindow>) {
    let pool = POOL.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let Some(backend) = backend else { return; };
    let me = backend.blocking_read().me.clone();
    if me.is_empty() {
        return;
    }
    let Some(pool) = pool else { return; };
    let rt = runtime();
    rt.spawn(async move {
        let me_id = sqlite::users::ensure_identity(&pool, &me).await.unwrap_or(0);
        let stored = sqlite::notes::list_notes(&pool, me_id).await.unwrap_or_default();
        let _ = slint::invoke_from_event_loop(move || {
            {
                let mut g = backend.blocking_write();
                for n in stored {
                    let key = g.key_for_note(n.id);
                    if !g.notes.iter().any(|x| x.note_id == n.id) {
                        g.notes.push(data::NoteRowData {
                            key,
                            note_id: n.id,
                            content: n.content,
                            created_at: n.created_at,
                        });
                    }
                }
            }
            if let Some(ui) = weak.upgrade() {
                publish_my_notes(&ui.global::<AppState>(), &backend);
            }
        });
    });
}

/// Persist a freshly-written note to SQLite, add it to the backend, publish.
fn save_my_note(weak: slint::Weak<MainWindow>, content: String) {
    let pool = POOL.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(pool), Some(backend)) = (pool, backend) else { return; };
    let me = backend.blocking_read().me.clone();
    if me.is_empty() || content.trim().is_empty() {
        return;
    }
    let rt = runtime();
    rt.spawn(async move {
        let me_id = match sqlite::users::ensure_identity(&pool, &me).await {
            Ok(id) => id,
            Err(_) => return,
        };
        let created = match sqlite::notes::create_note(&pool, me_id, &content).await {
            Ok(n) => n,
            Err(_) => return,
        };
        let _ = slint::invoke_from_event_loop(move || {
            {
                let mut g = backend.blocking_write();
                let now = created.updated_at.unwrap_or(created.created_at);
                let key = g.key_for_note(created.id);
                g.notes.insert(
                    0,
                    data::NoteRowData {
                        key,
                        note_id: created.id,
                        content: created.content,
                        created_at: now,
                    },
                );
            }
            if let Some(ui) = weak.upgrade() {
                publish_my_notes(&ui.global::<AppState>(), &backend);
            }
        });
    });
}

/// Delete a note by its stable UI key: remove from backend + SQLite, publish.
fn delete_my_note(weak: slint::Weak<MainWindow>, key: i32) {
    let pool = POOL.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(pool), Some(backend)) = (pool, backend) else { return; };
    let note_id = { backend.blocking_read().note_id_for(key) };
    let Some(note_id) = note_id else { return; };
    {
        let mut g = backend.blocking_write();
        g.notes.retain(|n| n.note_id != note_id);
    }
    let rt = runtime();
    rt.spawn(async move {
        let _ = sqlite::notes::delete_note(&pool, note_id).await;
    });
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            publish_my_notes(&ui.global::<AppState>(), &backend);
        }
    });
}

/// Resolve the picked contact keys to directory members, create the group in
/// core, announce + distribute the group key, then surface a conversation row
/// and open it. All core calls here use the shared `&self` interface.
fn create_group_flow(weak: slint::Weak<MainWindow>, picked: Vec<i32>) -> bool {
    eprintln!("[create_group_flow] picked={picked:?}");
    let client = CLIENT.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(client), Some(backend)) = (client, backend) else {
        eprintln!("[create_group_flow] missing client/backend");
        return false;
    };
    if picked.is_empty() {
        eprintln!("[create_group_flow] picked is empty");
        return false;
    }
    // Resolve each picked contact to its member public info via the directory.
    let mut members: Vec<MemberInfo> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for key in &picked {
        let username: Option<String> = {
            let g = backend.blocking_read();
            g.peer_id_for(*key).map(|s| s.to_string())
        };
        let Some(username) = username else {
            failed.push(format!("#{key}(本地id失效)"));
            continue;
        };
        match client.resolve_user_registered(&username) {
            Ok(ur) => {
                members.push(MemberInfo {
                    peer_id: ur.device.peer_id.clone(),
                    sign_pk: ur.user.sign_pk.clone(),
                    e2e_public: ur.user.e2e_public.clone(),
                });
            }
            Err(e) => {
                eprintln!("[create_group_flow] resolve_user({username}) failed: {e}");
                failed.push(format!("{username}({e})"));
            }
        }
    }
    if members.is_empty() && !failed.is_empty() {
        let msg = format!("无法解析用户设备：{}\n请确认对方账号已在目录注册。", failed.join(", "));
        eprintln!("[create_group_flow] {msg}");
        show_app_error(weak.clone(), String::from("无法解析用户设备"));
        return false;
    }
    if !failed.is_empty() {
        eprintln!("[create_group_flow] some members failed: {} (continuing with {} resolved)", failed.join(", "), members.len());
    }

    let group_id = format!("grp-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0));
    let title = format!("群聊({})", members.len());
    match client.create_group(&group_id, members) {
        Ok(_) => {
            let _ = client.announce_group(&group_id);
            let _ = client.publish_group_key(&group_id);
            // Insert a conversation row and open it.
            let new_key = {
                let mut g = backend.blocking_write();
                g.append_chat(group_id.clone(), title, true)
            };
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_group_status(SharedString::new());
                    publish_to_views(&state, backend.clone());
                    load_chat_messages(ui.as_weak(), new_key);
                    push_sub_history(&state, SubPageEntry { page: SubPageType::ChatRoom, payload: new_key });
                }
            });
            true
        }
        Err(e) => {
            let msg = "建群失败".to_string();
            eprintln!("{e}");
            eprintln!("[create_group_flow] {msg}");
            show_app_error(weak.clone(), msg);
            false
        }
    }
}

/// Search for a user on the server (by username) or locally (by scanned Peer
/// ID), then surface the profile in the AddContactView.
fn search_users(weak: slint::Weak<MainWindow>, q_raw: String) {
    let q: String = q_raw.chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return;
    }

    let pool = POOL.with(|s| s.borrow().clone());
    let client = CLIENT.with(|s| s.borrow().clone());
    match (pool, client) {
        (Some(pool), Some(client)) => {
            let my_peer = client.peer_base58();
        let rt = runtime();
        let q2 = q.clone();
        rt.spawn(async move {
            // (1) Try to resolve as a username against the directory.
            if let Ok(ur) = client.resolve_user(&q) {
                let uname = ur.user.user_id.clone();
                if let Ok(Some(user)) = sqlite::users::get_by_username(&pool, &uname).await {
                    let name = user.nickname.clone().unwrap_or_else(|| user.username.clone().unwrap_or_default());
                    let peer = sqlite::devices::get_by_peer(&pool, &my_peer).await.ok().flatten().map(|d| d.peer_id).unwrap_or(my_peer.clone());
                    let row = SearchUser {
                        name: SharedString::from(name),
                        username: SharedString::from(uname),
                        peer_id: SharedString::from(peer),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppState>().set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![row])));
                            ui.global::<AppState>().set_add_status(SharedString::new());
                        }
                    });
                    return;
                }
            }
            // (2) Try to resolve as a Peer ID (scan) against the directory.
            if let Ok(dev) = client.dir().resolve_device(&q) {
                let uname = dev.user_id.clone();
                if let Ok(Some(user)) = sqlite::users::get_by_username(&pool, &uname).await {
                    let name = user.nickname.clone().unwrap_or_else(|| user.username.clone().unwrap_or_default());
                    let row = SearchUser {
                        name: SharedString::from(name),
                        username: SharedString::from(uname),
                        peer_id: SharedString::from(q.clone()),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppState>().set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![row])));
                            ui.global::<AppState>().set_add_status(SharedString::new());
                        }
                    });
                    return;
                }
            }
            // (3) Fallback: local db only.
            if let Ok(Some(user)) = sqlite::users::get_by_username(&pool, &q2).await {
                let name = user.nickname.clone().unwrap_or_else(|| user.username.clone().unwrap_or_default());
                let row = SearchUser {
                    name: SharedString::from(name),
                    username: SharedString::from(q2.clone()),
                    peer_id: SharedString::new(),
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<AppState>().set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![row])));
                        ui.global::<AppState>().set_add_status(SharedString::new());
                    }
                });
                return;
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppState>().set_add_status(SharedString::from(format!("找不到用户: {q2}")));
                }
            });
        });
        }
        _ => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppState>().set_add_status(SharedString::from("尚未登录或后端未就绪"));
                }
            });
        }
    }
}

/// Mark `username` as a friend in the local DB (and refresh the in-memory
/// contact list so the UI updates).
fn add_contact(weak: slint::Weak<MainWindow>, username_raw: String) {
    let username: String = username_raw.chars().filter(|c| !c.is_whitespace()).collect();
    if username.is_empty() {
        return;
    }

    let backend = BACKEND.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let (Some(backend), Some(pool)) = (backend.clone(), pool) else {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_add_status(SharedString::from("尚未登录或后端未就绪"));
            }
        });
        return;
    };

    // Already a contact? Short-circuit before hitting sqlite.
    if backend.blocking_read().has_contact(&username, &username) {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_add_status(SharedString::from("已在通讯录中"));
            }
        });
        return;
    }
    let me = backend.blocking_read().me.clone();

    let rt = runtime();
    rt.spawn(async move {
        if let Err(e) = do_add(&pool, &me, &username, &backend).await {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppState>().set_add_status(SharedString::from(format!("添加失败: {e}")));
                }
            });
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let state = ui.global::<AppState>();
                publish_to_views(&state, backend.clone());
                state.set_add_status(SharedString::from("已添加到通讯录"));
            }
        });
    });
}

async fn do_add(pool: &sqlx::SqlitePool, me: &str, username: &str, backend: &ArcBackend) -> anyhow::Result<()> {
    let me_id = sqlite::users::ensure_identity(pool, me).await?;
    let their_id = sqlite::users::ensure_identity(pool, username).await?;
    sqlite::social::add(pool, me_id, their_id).await?;
    let (name, image) = {
        if let Ok(Some(u)) = sqlite::users::get(pool, their_id).await {
            (u.nickname.clone().unwrap_or_else(|| u.username.clone().unwrap_or_default()),
             u.avatar_path.clone().unwrap_or_default())
        } else {
            (username.to_string(), String::new())
        }
    };
    let mut g = backend.blocking_write();
    g.append_contact(their_id, username.to_string(), name, image);
    Ok(())
}

/// Show a transient global error dialog (transparent backdrop, message + 知道了).
fn show_app_error(weak: slint::Weak<MainWindow>, msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<AppState>();
            state.set_error_message(SharedString::from(msg));
            let mut nav = state.get_nav_state();
            nav.global_overlay = GlobalOverlayType::Error;
            state.set_nav_state(nav);
        }
    });
}

// ---------------------------------------------------------------------------
// Contact profile page: data load + actions
// ---------------------------------------------------------------------------

/// Switch to a global call overlay (audio / video / screen share).
fn show_call_overlay(weak: slint::Weak<MainWindow>, overlay: GlobalOverlayType) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<AppState>();
            let mut nav = state.get_nav_state();
            nav.global_overlay = overlay;
            state.set_nav_state(nav);
        }
    });
}

/// Push call-state into the global `CallState` (UI thread).
fn publish_call_state(
    weak: slint::Weak<MainWindow>,
    peer_base58: Option<String>,
    peer_name: Option<String>,
    active: bool,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let cs = ui.global::<CallState>();
            cs.set_active(active);
            if let Some(p) = peer_base58 {
                cs.set_peer_base58(SharedString::from(p));
            }
            if let Some(n) = peer_name {
                cs.set_peer_name(SharedString::from(n));
            }
        }
    });
}

/// Start a realtime voice call to the contact with the given `key`.
/// Resolves the peer, opens the call (mic + speaker via cpal), then shows the
/// in-call overlay. The inbound audio pump (started in `apply_result`) routes
/// any far-end frames into the speaker queue.
///
/// Must run on the UI thread — the Slint event loop owns `call::CALL` (a
/// thread_local) and cpal's streams.
fn begin_voice_call(weak: slint::Weak<MainWindow>, key: i32) {
    let client = CLIENT.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(client), Some(backend)) = (client, backend) else {
        publish_call_state(weak, None, None, false);
        return;
    };
    let client = Arc::new(client);

    let (peer_base58, peer_name) = {
        let g = backend.blocking_read();
        // `peer_id_for` returns the contact's base-58 peer id (what we use as
        // the directory username to resolve).
        let Some(pid) = g.peer_id_for(key).map(|s| s.to_string()) else {
            return;
        };
        let name = g
            .contacts
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| pid.clone());
        (pid, name)
    };

    // Resolve so we can confirm they're registered; `dial_peer` (inside
    // `start_call`) needs the directory entry to exist.
    if let Err(e) = client.resolve_user(&peer_base58) {
        eprintln!("[chat] begin_voice_call: resolve {peer_base58}: {e}");
        let weak2 = weak.clone();
        let name = peer_name.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak2.upgrade() {
                ui.set_auth_message(SharedString::from(format!("对端离线: {name}")));
            }
        });
        return;
    }

    match call::start_call(Arc::clone(&client), &peer_base58) {
        Ok(()) => {
            publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
            show_call_overlay(weak, GlobalOverlayType::AudioCall);
        }
        Err(e) => {
            publish_call_error(weak, format!("通话开始失败: {e}"));
        }
    }
}

fn publish_call_error(weak: slint::Weak<MainWindow>, msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_auth_message(SharedString::from(msg));
        }
    });
}

/// Hang up the active call (if any) and clear the in-call overlay.
///
/// Runs on the UI thread (same reasoning as `begin_voice_call`).
fn end_voice_call(weak: slint::Weak<MainWindow>) {
    let had_active = call::is_active();
    call::stop_call();
    if had_active {
        publish_call_state(weak.clone(), None, None, false);
        show_call_overlay(weak, GlobalOverlayType::None);
    }
}

/// Shared screen-frame funnel. One handler covers both paths:
/// - desktop `Screen::set_sink` (RGBA8888, no fmt) wraps its `(Vec<u8>,w,h)`
///   sink with a tiny closure that forwards into this.
/// - iOS / Android arrive via `bridge::set_screen_consumer` with the native
///   `fmt` (1 = RGBA8888 for MediaProjection, 7 = BGRA8888 for RPScreenRecorder).
fn on_screen_frame(_bytes: &[u8], w: u32, h: u32, fmt: u32) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    if n % 30 == 0 {
        eprintln!("[screen] frame #{} {}x{} fmt={}", n, w, h, fmt);
    }
}

/// Desktop-only wrapper: adapt the `Screen` sink's owned `Vec<u8>` to the
/// shared handler (which takes `&[u8]`).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn screen_sink_adapter(bytes: Vec<u8>, w: u32, h: u32) {
    on_screen_frame(&bytes, w, h, /*RGBA8888*/ 1);
}

/// Begin a screen share: on desktop this starts the local [`screen`] capture
/// crate and routes its frames into the unified handler; on iOS / Android the
/// capture itself is done by the native shell (ReplayKit / MediaProjection)
/// which already funnels into the bridge sink. Either way the overlay comes up.
fn begin_screen_share(weak: slint::Weak<MainWindow>) {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        SCREEN.with(|slot| {
            if slot.borrow().is_none() {
                *slot.borrow_mut() = Some(Screen::new());
            }
            let mut guard = slot.borrow_mut();
            let scr = guard.as_mut().expect("screen inited");
            if !scr.is_running() {
                scr.set_sink(screen_sink_adapter);
                if let Err(err) = scr.start() {
                    eprintln!("[screen] start failed: {err}");
                }
            }
        });
    }
    #[cfg(target_os = "android")]
    {
        // Ask the Java shell to launch the MediaProjection consent flow +
        // VirtualDisplay capture. Frames then arrive via
        // `NativeBridge.screenFrameIn` → bridge sink → `handle_screen_frame`.
        eprintln!("[screen] requesting MediaProjection from Java shell");
        bridge::request_screen_share_start();
    }
    #[cfg(target_os = "ios")]
    {
        // Ask the iOS shell (RPScreenRecorder) to start the capture. Frames
        // land via `bridge_screen_frame_in` → `handle_screen_frame`.
        eprintln!("[screen] requesting RPScreenRecorder from ObjC shim");
        bridge::request_screen_share_start();
    }
    show_call_overlay(weak, GlobalOverlayType::ScreenShare);
}

/// Stop the active screen share and clear the overlay.
fn end_screen_share(weak: slint::Weak<MainWindow>) {
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        SCREEN.with(|slot| {
            if let Some(s) = slot.borrow_mut().as_mut() {
                s.stop();
            }
        });
    }
    #[cfg(target_os = "android")]
    {
        eprintln!("[screen] stopping MediaProjection capture");
        bridge::request_screen_share_stop();
    }
    #[cfg(target_os = "ios")]
    {
        eprintln!("[screen] stopping RPScreenRecorder capture");
        bridge::request_screen_share_stop();
    }
    show_call_overlay(weak, GlobalOverlayType::None);
}

/// Convert a unix timestamp (ms or s) to `YYYY-MM-DD`.
fn fmt_profile_date(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let t = if ms > 10_000_000_000 { ms / 1000 } else { ms };
    let (y, m, d) = civil_date((t / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days-since-epoch to Y/M/D (Howard Hinnant's civil_from_days).
fn civil_date(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_1;
    let doe = (z - era * 146_1).unsigned_abs();
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as i64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as i64, d)
}

/// Publish a status-only update (key is set, data fields left blank) for a
/// given contact key.
fn publish_profile_status(weak: slint::Weak<MainWindow>, key: i32, msg: SharedString) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let ps = ui.global::<ProfileState>();
            ps.set_key(key);
            ps.set_status(msg);
        }
    });
}

fn load_profile_data(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_profile_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let Some(user_id) = backend.blocking_read().contact_id_for(key) else {
        publish_profile_status(weak, key, SharedString::from("联系人已失效"));
        return;
    };
    // Real peer id (username key used for directory lookups) for display / DM.
    let peer_id = backend
        .blocking_read()
        .peer_id_for(key)
        .map(|s| s.to_string())
        .unwrap_or_default();

    // Each request gets a fresh id; a stale one is dropped before publishing.
    let seq = PROFILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    PROFILE_LATEST.store(seq, std::sync::atomic::Ordering::SeqCst);
    let my_key = key;

    let rt = runtime();
    let weak2 = weak.clone();
    rt.spawn(async move {
        let st = match sqlite::users::get(&pool, user_id).await {
            Ok(Some(u)) => u,
            Ok(None) => {
                if PROFILE_LATEST.load(std::sync::atomic::Ordering::SeqCst) == seq {
                    publish_profile_status(weak2, my_key, SharedString::from("该用户未在本机同步"));
                }
                return;
            }
            Err(e) => {
                if PROFILE_LATEST.load(std::sync::atomic::Ordering::SeqCst) == seq {
                    publish_profile_status(weak2, my_key, SharedString::from(format!("读取失败: {e}")));
                }
                return;
            }
        };
        let (avatar_path, name, username, bio, created_at, moment) = {
            let m = sqlite::joins::author_feed(&pool, user_id, 1, 0)
                .await
                .ok()
                .and_then(|v| v.into_iter().next());
            (
                st.avatar_path.clone().unwrap_or_default(),
                st.nickname
                    .clone()
                    .unwrap_or_else(|| st.username.clone().unwrap_or_default()),
                st.username.clone().unwrap_or_default(),
                st.bio.clone().unwrap_or_default(),
                st.created_at,
                m.map(|m| {
                    (
                        m.content.unwrap_or_default(),
                        m.timestamp,
                        m.like_count as i32,
                    )
                }),
            )
        };
        // Drop if a newer request superseded this one.
        if PROFILE_LATEST.load(std::sync::atomic::Ordering::SeqCst) != seq {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak2.upgrade() else { return };
            let ps = ui.global::<ProfileState>();
            let avatar = slint::Image::load_from_path(std::path::Path::new(&avatar_path))
                .unwrap_or_default();
            let (has_moment, mt, mtime, ml) = match moment {
                Some((text, ts, likes)) => (true, text, ts, likes),
                None => (false, String::new(), 0, 0),
            };
            ps.set_key(my_key);
            ps.set_avatar(avatar);
            ps.set_name(SharedString::from(name));
            ps.set_username(SharedString::from(username));
            ps.set_peer_id(SharedString::from(peer_id));
            ps.set_bio(SharedString::from(bio));
            ps.set_created_at(SharedString::from(fmt_profile_date(created_at)));
            ps.set_has_moment(has_moment);
            ps.set_moment_text(SharedString::from(mt));
            ps.set_moment_time(SharedString::from(fmt_profile_date(mtime)));
            ps.set_moment_likes(ml);
            ps.set_status(SharedString::new());
        });
    });
}

/// Open (or create) a 1:1 conversation row with the picked contact and
/// navigate to it.
fn open_conversation_with_contact(weak: slint::Weak<MainWindow>, key: i32) {
    let client = CLIENT.with(|s| s.borrow().clone());
    let backend = BACKEND.with(|s| s.borrow().clone());
    let (Some(client), Some(backend)) = (client, backend) else {
        publish_profile_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let username = {
        let g = backend.blocking_read();
        g.peer_id_for(key).map(|s| s.to_string())
    };
    let Some(username) = username else {
        publish_profile_status(weak, key, SharedString::from("联系人已失效"));
        return;
    };
    let Some(ur) = client.resolve_user(&username).ok() else {
        publish_profile_status(weak, key, SharedString::from("该用户未注册，无法发起会话"));
        return;
    };
    let me_peer = client.peer_base58();
    let chat_id = dm_chat_id(&me_peer, &ur.device.peer_id);
    let title = ur.user.user_id.clone();
    let new_key = match backend.blocking_read().chats.iter().find(|c| c.chat_id == chat_id).map(|c| c.key) {
        Some(k) => k,
        None => {
            let mut g = backend.blocking_write();
            g.append_chat(chat_id, title, false)
        }
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<AppState>();
            publish_to_views(&state, backend.clone());
            load_chat_messages(ui.as_weak(), new_key);
            push_sub_history(&state, SubPageEntry { page: SubPageType::ChatRoom, payload: new_key })
        }
    });
}

// ---------------------------------------------------------------------------
// Discover post detail page: load / like / comment
// ---------------------------------------------------------------------------

/// Publish a status-only update for a post-detail key (data fields untouched).
fn publish_detail_status(weak: slint::Weak<MainWindow>, key: i32, msg: SharedString) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let ps = ui.global::<PostDetailState>();
            ps.set_key(key);
            ps.set_status(msg);
        }
    });
}

fn detail_key_post(backend: &ArcBackend, key: i32) -> Option<i64> {
    backend.blocking_read().post_id_for(key)
}

/// Parse the `social_posts.media_urls` column into a list of URLs/paths.
/// Accepts either a JSON array (`["a.jpg","b.jpg"]`) or a single raw path/URL
/// (no brackets, no commas). Returns an empty vec on any failure.
fn parse_media_urls(s: &str) -> Vec<String> {
    let t = s.trim();
    if t.is_empty() {
        return Vec::new();
    }
    // Try JSON array first; fall back to treating as a single path.
    if let Ok(v) = serde_json::from_str::<Vec<String>>(t) {
        let v: Vec<String> = v.into_iter().filter(|x| !x.trim().is_empty()).collect();
        if !v.is_empty() {
            return v;
        }
    }
    // Single path/URL (no brackets).
    if !t.starts_with('[') {
        return vec![t.to_string()];
    }
    Vec::new()
}

/// Load a post's detail (post, like state, likers, comments) from SQLite and
/// publish it into the PostDetailState global. A monotonic sequence guard drops
/// out-of-order async results so a stale load can't clobber the newest one.
fn load_post_detail(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let Some(post_id) = detail_key_post(&backend, key) else {
        publish_detail_status(weak, key, SharedString::from("帖子已失效"));
        return;
    };

    let (author, me) = {
        let g = backend.blocking_read();
        (
            g.discover
                .iter()
                .find(|r| r.key == key)
                .map(|r| r.author.clone())
                .unwrap_or_default(),
            g.me.clone(),
        )
    };

    let seq = POST_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    POST_LATEST.store(seq, std::sync::atomic::Ordering::SeqCst);

    let rt = runtime();
    let weak2 = weak.clone();
    rt.spawn(async move {
        let post = match sqlite::social_feed::get_post(&pool, post_id).await {
            Ok(Some(p)) => p,
            _ => {
                if POST_LATEST.load(std::sync::atomic::Ordering::SeqCst) == seq {
                    publish_detail_status(weak2, key, SharedString::from("帖子不存在"));
                }
                return;
            }
        };

        let me_id = sqlite::users::ensure_identity(&pool, &me).await.unwrap_or(0);
        let i_liked = sqlite::social_feed::has_liked(&pool, post_id, me_id).await.unwrap_or(false);
        // Resolve each liker to a display name for the "who liked" line.
        let mut likers: Vec<String> = Vec::new();
        if let Ok(ids) = sqlite::social_feed::likers(&pool, post_id).await {
            for uid in ids.iter().cloned().take(20) {
                let name = match sqlite::users::get(&pool, uid).await {
                    Ok(Some(u)) => u
                        .nickname
                        .clone()
                        .unwrap_or_else(|| u.username.clone().unwrap_or_default()),
                    _ => uid.to_string(),
                };
                if !name.is_empty() {
                    likers.push(name);
                }
            }
        }
        let likers_line = if likers.is_empty() {
            String::new()
        } else {
            let head = likers.join("、");
            if likers.len() > 5 {
                format!("{} 等 {} 人觉得很赞", head, likers.len())
            } else {
                format!("{} 觉得很赞", head)
            }
        };

        let comments = match sqlite::joins::post_comments(&pool, post_id, 200, 0).await {
            Ok(rows) => rows,
            Err(e) => {
                if POST_LATEST.load(std::sync::atomic::Ordering::SeqCst) == seq {
                    publish_detail_status(weak2, key, SharedString::from(format!("读取评论失败: {e}")));
                }
                return;
            }
        };

        // media_urls may be a JSON array of URLs/paths or a single path; keep the
        // raw strings for the UI thread to load (slint::Image is not Send).
        let media_urls = match post.media_urls.clone() {
            Some(s) if !s.trim().is_empty() => parse_media_urls(&s),
            _ => Vec::new(),
        };

        // Drop if a newer request superseded this one.
        if POST_LATEST.load(std::sync::atomic::Ordering::SeqCst) != seq {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak2.upgrade() else { return };
            let ps = ui.global::<PostDetailState>();
            ps.set_key(key);
            ps.set_title(SharedString::from(post.content.clone().unwrap_or_default()));
            ps.set_author(SharedString::from(author));
            ps.set_time(SharedString::from(fmt_time(post.timestamp)));
            ps.set_liked(i_liked);
            ps.set_likes(post.like_count as i32);
            ps.set_comments(post.comment_count as i32);
            ps.set_liked_by(SharedString::from(likers_line));
            // Load media on the UI thread (slint::Image is not Send).
            let media: Vec<SlintImage> = media_urls
                .into_iter()
                .filter(|s| !s.is_empty())
                .filter_map(|p| SlintImage::load_from_path(std::path::Path::new(&p)).ok())
                .collect();
            ps.set_media(slint::ModelRc::new(slint::VecModel::from(media)));
            let rows: Vec<CommentRow> = comments
                .iter()
                .map(|c| CommentRow {
                    name: SharedString::from(
                        c.author_nickname
                            .clone()
                            .unwrap_or_else(|| c.author_id.to_string()),
                    ),
                    text: SharedString::from(c.content.clone()),
                    is_mine: me_id != 0 && c.author_id == me_id,
                    time_label: SharedString::from(fmt_time(c.created_at)),
                })
                .collect();
            ps.set_comment_list(slint::ModelRc::new(slint::VecModel::from(rows)));
            ps.set_status(SharedString::new());
        });
    });
}

/// Toggle a like on the post shown in the detail view: memory first (so the
/// count/heart update immediately), then async persist to SQLite.
fn toggle_detail_like(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let post_id = detail_key_post(&backend, key);
    let (want_liked, likes) = {
        let mut g = backend.blocking_write();
        let (liked, likes) = g.toggle_like(key);
        (liked, likes)
    };
    // Refresh memory -> UI (updates the discover feed + the detail view).
    if let Some(ui) = weak.upgrade() {
        publish_to_views(&ui.global::<AppState>(), backend.clone());
        let ps = ui.global::<PostDetailState>();
        ps.set_liked(want_liked);
        ps.set_likes(likes as i32);
    }
    if let Some(post_id) = post_id {
        let rt = runtime();
        let me = backend.blocking_read().me.clone();
        rt.spawn(async move {
            let _ = data::persist_like_toggle(&pool, &me, post_id, want_liked).await;
        });
    }
}

/// Add a comment to the post shown in the detail view: persist to SQLite,
/// prepend it to the comment list, and bump the comment count.
fn add_post_comment(weak: slint::Weak<MainWindow>, key: i32, body: String) {
    let backend = BACKEND.with(|s| s.borrow().clone());
    let pool = POOL.with(|s| s.borrow().clone());
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let Some(post_id) = detail_key_post(&backend, key) else {
        publish_detail_status(weak, key, SharedString::from("帖子已失效"));
        return;
    };
    let (my_name, me) = {
        let g = backend.blocking_read();
        let n = g.my_nickname.clone();
        let me = g.me.clone();
        (if n.is_empty() { me.clone() } else { n }, me)
    };
    let rt = runtime();
    rt.spawn(async move {
        let me_id = match sqlite::users::ensure_identity(&pool, &me).await {
            Ok(id) => id,
            Err(e) => {
                publish_detail_status(weak.clone(), key, SharedString::from(format!("评论失败: {e}")));
                return;
            }
        };
        match sqlite::social_feed::add_comment(&pool, post_id, me_id, &body, None).await {
            Ok(_) => {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let ps = ui.global::<PostDetailState>();
                    let mut list: Vec<CommentRow> = {
                        use slint::Model;
                        let m = ps.get_comment_list();
                        (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                    };
                    let new = CommentRow {
                        name: SharedString::from(my_name),
                        text: SharedString::from(body),
                        is_mine: true,
                        time_label: SharedString::from("刚刚"),
                    };
                    list.insert(0, new);
                    ps.set_comment_list(slint::ModelRc::new(slint::VecModel::from(list)));
                    ps.set_comments((ps.get_comments() as i64 + 1) as i32);
                    ps.set_status(SharedString::new());
                });
            }
            Err(e) => {
                publish_detail_status(weak, key, SharedString::from(format!("评论失败: {e}")));
            }
        }
    });
}
