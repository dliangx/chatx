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
pub mod call_dispatcher;

// `camera::Camera` exists on every target (Apple: AVFoundation impl, others:
// no-op fallback whose `start()` reports "unsupported"), so import it
// unconditionally. The *usage* below is still gated to Apple.
use camera::Camera;
use screen::Screen;

slint::include_modules!();

type Directory = HttpDirectory;

// Shared, cross-thread data globals.
// Must NOT be thread_local! because they are read from tokio worker threads
// (inside `rt.spawn(async move { … })` blocks), not just from the UI thread.
static CLIENT: std::sync::Mutex<Option<Arc<Client<Directory>>>> = std::sync::Mutex::new(None);
static POOL: std::sync::Mutex<Option<Arc<sqlx::SqlitePool>>> = std::sync::Mutex::new(None);
static BACKEND: std::sync::Mutex<Option<ArcBackend>> = std::sync::Mutex::new(None);

thread_local! {
    static RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> = RefCell::new(None);
    static PUMP_STARTED: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static CAMERA: RefCell<Option<Camera>> = RefCell::new(None);
    static SCREEN: RefCell<Option<Screen>> = RefCell::new(None);
    /// `true` iff the current device was logged in as an APPROVED device.
    /// Recomputed on login/apply_result and after approving this device.
    static DEVICE_APPROVED: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// True if the current device is approved (allow user actions); false while it
/// is pending approval by another authenticated device of the same account.
fn device_is_approved() -> bool {
    DEVICE_APPROVED.with(|c| c.get())
}

/// Shared renderer, initialized lazily (font parsing is expensive) and shared
/// across worker threads via a mutex so it is only built once.
static RENDERER: std::sync::OnceLock<std::sync::Mutex<Renderer>> = std::sync::OnceLock::new();

/// Per-conversation "distance-from-bottom" of the last scroll position the
/// user viewed, in logical px (0 = fully at the newest message). Restored when
/// the chat view (re)enters so the user picks up where they left off.
static CHAT_SCROLL_POS: std::sync::LazyLock<std::sync::RwLock<std::collections::HashMap<i32, f32>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Monotonic id for profile loads (each request gets a fresh id).
static PROFILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Id of the most-recent profile load; stale ones are dropped before publishing.
static PROFILE_LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Monotonic id for post-detail loads (each request gets a fresh id).
static POST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static IMAGE_CACHE: std::cell::RefCell<std::collections::HashMap<String, SlintImage>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn load_image_cached(path: &str) -> SlintImage {
    let key = path.to_string();
    let cached = IMAGE_CACHE.with(|c| c.borrow().get(&key).cloned());
    if let Some(img) = cached {
        return img;
    }
    let img = SlintImage::load_from_path(std::path::Path::new(&key)).unwrap_or_default();
    IMAGE_CACHE.with(|c| c.borrow_mut().insert(key, img.clone()));
    img
}
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
    call::set_weak_window(weak.clone());
    state.set_user_id(SharedString::from(""));
    state.set_is_mobile(cfg!(target_os = "android") || cfg!(target_os = "ios"));
    // state.set_is_mobile(true);
    let existing_uid = chatx_core::account::Keystore::load(&keystore_path(&profile()))
        .map(|ks| ks.user_id)
        .ok();
    if let Some(uid) = existing_uid {
        ui.global::<AppState>().set_auth_message(SharedString::from("please logging in "));
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
        ui.global::<AppState>().on_login(move |user_id, pass| {
            let uid = user_id.to_string();
            let pass = pass.to_string();
            let profile = profile();
            let dir = default_server();
            let weak = weak.clone();
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<AppState>();
                st.set_auth_busy(true);
                st.set_auth_message(SharedString::from("logging in …"));
            }
            let rt = runtime();
            rt.spawn(async move {
                let res = Client::login(&profile, &pass, uid.clone(), dir).await;
                if res.is_ok() {
                    save_passphrase(&uid, &pass);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<AppState>().set_auth_busy(false);
                    }
                    apply_result(weak, uid, res);
                });
            });
        });
    }

    {
        let weak = weak.clone();
        ui.global::<AppState>().on_register(move |user_id, pass| {
            let uid = user_id.to_string();
            let pass = pass.to_string();
            let profile = profile();
            let dir = default_server();
            let weak = weak.clone();
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<AppState>();
                st.set_auth_busy(true);
                st.set_auth_message(SharedString::from("register …"));
            }
            let rt = runtime();
            rt.spawn(async move {
                let res = Client::bootstrap(&profile, uid.clone(), &pass, dir).await;
                if res.is_ok() {
                    save_passphrase(&uid, &pass);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<AppState>().set_auth_busy(false);
                    }
                    apply_result(weak, uid, res);
                });
            });
        });
    }

    let w = weak.clone();
    ui.global::<AppState>().on_logout(move || {
        *CLIENT.lock().unwrap() = None;
        *POOL.lock().unwrap() = None;
        *BACKEND.lock().unwrap() = None;
        PUMP_STARTED.set(false);
        DEVICE_APPROVED.with(|c| c.set(false));
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
            st.set_my_devices(slint::ModelRc::new(slint::VecModel::from(Vec::<MyDevice>::new())));
            st.set_device_approved(true);
            st.set_notice_text(SharedString::new());
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
        use slint::Model;
        let w = weak.clone();
        ui.global::<AppState>().on_pick_group_member(move |key| {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let m = st.get_group_picked();
                let mut picked: Vec<i32> = (0..m.row_count()).filter_map(|i| m.row_data(i)).collect();
                if !picked.contains(&key) {
                    picked.push(key);
                    st.set_group_picked(slint::ModelRc::new(slint::VecModel::from(picked)));
                }
            }
        });
    }
    {
        use slint::Model;
        let w = weak.clone();
        ui.global::<AppState>().on_unpick_group_member(move |key| {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let m = st.get_group_picked();
                let mut picked: Vec<i32> = (0..m.row_count()).filter_map(|i| m.row_data(i)).collect();
                picked.retain(|k| *k != key);
                st.set_group_picked(slint::ModelRc::new(slint::VecModel::from(picked)));
            }
        });
    }
    {
        use slint::Model;
        let w = weak.clone();
        ui.global::<AppState>().on_create_group(move || {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let m = st.get_group_picked();
                let picked: Vec<i32> = (0..m.row_count()).filter_map(|i| m.row_data(i)).collect();
                if create_group_flow(w.clone(), picked) {
                    st.set_group_picked(slint::ModelRc::new(slint::VecModel::from(Vec::<i32>::new())));
                }
            }
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_show_qr_code(move |peer: SharedString| {
            let peer = peer.to_string();
            eprintln!("[qr] on_show_qr_code: target={peer}");
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                st.set_qr_target_id(SharedString::from(peer.clone()));
                st.set_qr_image(qr_image(&peer));
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

    // ---- Device approval ----
    {
        let w = weak.clone();
        ui.global::<AppState>().on_refresh_devices(move || {
            refresh_devices(w.clone());
        });
    }
    {
        let w = weak.clone();
        ui.global::<AppState>().on_approve_device(move |peer| {
            approve_device(w.clone(), peer.to_string());
        });
    }
    {
        let w = weak.clone();
        ui.global::<AppState>().on_revoke_device(move |peer| {
            revoke_device(w.clone(), peer.to_string());
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
            eprintln!("[chat] on_open_conversation: got key={key}");
            open_conversation_with_contact(weak.clone(), key);
        });
    }
       {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_start_audio_call(move |key| {
            begin_voice_call(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_start_video_call(move |key| {
            begin_video_call(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_start_screen_share(move |key| {
            begin_screen_share(weak.clone(), key);
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
        ui.global::<CallState>().on_toggle_mute(move || {
            match weak.upgrade() {
                Some(ui) => {
                    let st = ui.global::<CallState>();
                    let new = !st.get_muted();
                    st.set_muted(new);
                    call::mute_mic(new);
                }
                None => {}
            }
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

    {
        ui.global::<ChatSession>().on_update_scroll(move |key, dist| {
            CHAT_SCROLL_POS.write().unwrap().insert(key, dist);
        });
    }

    {
        let weak = weak.clone();
        ui.global::<ChatSession>().on_scroll_request(move |key| {
            // A freshly-mounted view (e.g. re-entering the chat after a tab
            // switch) asks Rust to restore its remembered position from
            // memory. Re-bumping the token makes the view re-apply
            // `scroll-dist` even if the user had scrolled up before leaving.
            let dist = CHAT_SCROLL_POS.read().unwrap().get(&key).copied().unwrap_or(0.0);
            if let Some(ui) = weak.upgrade() {
                let cs = ui.global::<ChatSession>();
                cs.set_scroll_dist(dist);
                cs.set_scroll_token(cs.get_scroll_token() + 1);
            }
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
            eprintln!("[scan] on_scanner_start → opening camera");
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
            call_dispatcher::clear_qr_hook();
            if let Some(ui) = w.upgrade() {
                ui.global::<AppState>().set_scan_status(SharedString::new());
                let st = ui.global::<AppState>();
                st.set_scan_preview(slint::Image::default());
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
    static AUDIO_N: AtomicU64 = AtomicU64::new(0);

    // Mobile mic → webrtc. `bridge::set_audio_consumer` is the **inbound mic
    // PCM** hook (`bridge_audio_pcm_in` from AVAudioEngine tap / AudioRecord),
    // i.e. local-mic → peer. Route straight into the dispatcher, which pushes
    // it into the active call's `AudSource` (no-op while no call).
    bridge::set_audio_consumer(Box::new(move |bytes, rate, ch| {
        let n = AUDIO_N.fetch_add(1, Ordering::Relaxed);
        if n % 100 == 0 {
            eprintln!("[bridge] mic #{n} {}B @{}Hz/{}ch", bytes.len(), rate, ch);
        }
        call_dispatcher::on_audio_frame(bytes, rate);
    }));
    // Mobile screen frames (ReplayKit/MediaProjection) — the dispatcher
    // converts any `fmt` to RGBA8888 before pushing.
    bridge::set_screen_consumer(Box::new(move |bytes, w, h, fmt| {
        call_dispatcher::on_screen_frame(bytes.to_vec(), w, h, fmt);
    }));
    // Mobile camera frames (JNI/ObjC shim) — routed through the dispatcher's
    // camera-frame sink so an active call's `VidSource` + local preview +
    // QR scanner all see them.
    bridge::set_camera_consumer(Box::new(move |bytes, w, h, fmt| {
        call_dispatcher::on_camera_frame(bytes, w, h, fmt);
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

/// Open the camera and wire decoded QR payloads + raw preview frames into
/// the scan UI.
fn start_camera_scanner(weak: slint::Weak<MainWindow>, state: AppState) {
    CAMERA.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Camera::new());
        }
        let mut guard = slot.borrow_mut();
        let cam = guard.as_mut().expect("camera inited");
        if cam.is_running() {
            eprintln!("[scan] camera already running, skipping start");
            return;
        }
        eprintln!("[scan] calling cam.start() …");
        match cam.start() {
            Ok(_) => {
                eprintln!("[scan] cam.start() OK — installing QR sink + preview sink");
                let wsink = weak.clone();
                let wsink2 = weak.clone();
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

                let latest_seen = std::sync::atomic::AtomicU64::new(0);
                let frame_n = std::sync::atomic::AtomicU64::new(0);
                call_dispatcher::install_qr_hook(move |bytes, w, h, fmt| {
                    const GATE_MS: u64 = 30; // ~30 fps ceiling
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    loop {
                        let last = latest_seen.load(std::sync::atomic::Ordering::Relaxed);
                        if now.saturating_sub(last) < GATE_MS {
                            return;
                        }
                        match latest_seen.compare_exchange_weak(
                            last,
                            now,
                            std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(_) => continue,
                        }
                    }
                    let n = frame_n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 3 {
                        eprintln!("[scan.preview] app sink #{n}: {w}x{h} fmt={fmt} bytes={}", bytes.len());
                    }
                    let data = bytes.to_vec();
                    let wk = wsink2.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(ui) = wk.upgrade() else { return; };
                        let st = ui.global::<AppState>();
                        st.set_scan_preview(make_preview_image(&data, w, h, fmt));
                    });
                });
                // Install the dispatcher's camera sink (single, global).
                cam.set_frame_sink(call_dispatcher::on_camera_frame);
            }
            Err(err) => {
                state.set_scan_status(SharedString::from(format!("无法启动摄像头: {err}")));
            }
        }
    });
}

fn make_preview_image(data: &[u8], w: u32, h: u32, fmt: u32) -> slint::Image {
    if w == 0 || h == 0 {
        return slint::Image::default();
    }
    let n = (w as usize) * (h as usize);
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    let dst = buf.make_mut_bytes();
    match fmt {
        1 => {
            // Already RGBA.
            copy_rgba(data, dst, n);
        }
        8 => {
            // Expand Y → R=G=B.
            let mut di = 0usize;
            for &y in data.iter().take(n) {
                if di + 4 > dst.len() { break; }
                dst[di] = y;
                dst[di + 1] = y;
                dst[di + 2] = y;
                dst[di + 3] = 255;
                di += 4;
            }
        }
        _ => {
            // Unknown → leave the zero-initialised buffer (black).
        }
    }
    slint::Image::from_rgba8(buf)
}

fn copy_rgba(src: &[u8], dst: &mut [u8], n: usize) {
    let want = n * 4;
    let len = want.min(src.len()).min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
}

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
            // Re-touch the directory every 15s so the device stays "online"
            // (server TTL is 30s) and our egress IP is re-published whenever
            // the local interface changes (WiFi ↔ ethernet, VPN, …).
            client.start_presence_loop();
            let approved = client
                .status()
                .ok()
                .map(|s| s == chatx_core::account::DeviceStatus::Approved)
                .unwrap_or(false);
            eprintln!("[login] device status approved={approved}");
            DEVICE_APPROVED.with(|c| c.set(approved));
            *CLIENT.lock().unwrap() = Some(client);

            start_inbound_pump(weak.clone());

            if let Some(pool) = pool {
                let pool = Arc::new(pool);
                *POOL.lock().unwrap() = Some(pool.clone());
                let rt = runtime();
                let weak = weak.clone();
                let me = user_id.clone();
                rt.spawn(async move {
                    match data::load(&pool, &me).await {
                        Ok((backend, nickname)) => {
                            let backend = Arc::new(tokio::sync::RwLock::new(backend));
                            let peer = peer.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                *BACKEND.lock().unwrap() = Some(backend.clone());
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
                ui.global::<AppState>().set_auth_message(SharedString::from(err.to_string()));
                ui.set_logged_in(false);
            }
            return;
        }
    }

    if let Some(ui) = weak.upgrade() {
        ui.global::<AppState>().set_auth_message(SharedString::from(format!("logged in:{user_id}")));
        ui.global::<AppState>().set_user_id(SharedString::from(user_id));
        ui.set_logged_in(true);
        refresh_devices(ui.as_weak());
    }
}

fn start_inbound_pump(weak: slint::Weak<MainWindow>) {
    if PUMP_STARTED.get() {
        return;
    }
    PUMP_STARTED.set(true);
    let weak = weak.clone();
    let rt = runtime();
    rt.spawn(async move {
        loop {
            let client: std::sync::Arc<chatx_core::Client<chatx_core::signal::HttpDirectory>> = match CLIENT.lock().unwrap().clone() {
                Some(c) => c,
                None => break,
            };
            let Some(evt) = client.next_event().await else {
                break;
            };
            // ── Inbound WebRTC signaling: answer Offer or apply Answer/ICE
            // on the active call. `handle_signal_offer` blocks the UI
            // thread while it does the offer/answer handshake; acceptable
            // (rare and brief). ────────────────────────────────────────────
            if let chatx_core::swarm::ChatEvent::Webrtc { peer, json, .. } = &evt {
                let client_arc = client.clone();
                let peer = *peer;
                let json = json.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    call::handle_signal_offer(client_arc, peer, &json);
                });
                continue;
            }
            // Note: `ChatEvent::Audio` is intentionally not handled — the
            // `PeerCall` remote sink (installed by `call::wire_remote_sinks`)
            // already routes inbound peer audio into the cpal speaker queue.
            let touched = client.process_event(evt).await;
            if let Some(touched) = touched {
                refresh_inbound_chat(weak.clone(), touched).await;
            }
        }
    });
}

/// Inbound message refresh: update the DB row + chat list preview, and if the
/// conversation is currently open, re-render its messages.
///
/// This is invoked from the tokio worker task of the inbound pump loop, so we
/// cannot call `backend.blocking_read()` (which is used by `publish_to_views`)
/// on this thread. The UI-facing work is dispatched to the event-loop thread
/// where it is legal.
async fn refresh_inbound_chat(weak: slint::Weak<MainWindow>, chat_id: String) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else {
        return;
    };
    let me = weak.clone().upgrade().and_then(|ui| Some(ui.global::<AppState>().get_user_id().to_string())).unwrap_or_default();
    if me.is_empty() {
        return;
    }
    {
        let mut b: data::DataBackend = backend.read().await.clone();
        let _ = data::refresh_chat_row(&pool, &me, &mut b, &chat_id).await;
        *backend.write().await = b;
    }
    let is_open_key = {
        backend.read().await.chats.iter().find(|r| r.chat_id == chat_id).map(|r| r.key)
    };
    let weak2 = weak.clone();
    let backend2 = backend.clone();
    let key = is_open_key;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak2.upgrade() {
            let state = ui.global::<AppState>();
            publish_to_views(&state, backend2.clone());
            if let Some(k) = key {
                use slint::Model;
                let nav = state.get_nav_state();
                let tabs = nav.tabs.clone();
                if let Some(tab) = tabs.row_data(nav.active_tab as usize) {
                    let top = tab.sub_top;
                    if top >= 0 {
                        let hist = tab.sub_history.clone();
                        if let Some(entry) = hist.row_data(top as usize) {
                            if entry.page == SubPageType::ChatRoom && entry.payload == k {
                                load_chat_messages(ui.as_weak(), k);
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Push the in-memory backend rows into the slint view models (memory -> UI).
fn publish_to_views(state: &AppState, backend: ArcBackend) {
    let mut snapshot = backend.blocking_read().clone();

    let rank = |c: char| if c == '#' { '[' } else { c };
    snapshot
        .contacts
        .sort_by(|a, b| rank(data::contact_letter(&a.name))
            .cmp(&rank(data::contact_letter(&b.name)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));

    let mut chats_sorted = snapshot.chats.clone();
    chats_sorted.sort_by(|a, b| b.time_ms.cmp(&a.time_ms).then(b.key.cmp(&a.key)));
    let chats: Vec<ConversationRow> = chats_sorted
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
            image: load_image_cached(&r.image),
            peer_id: SharedString::from(r.peer_id.clone()),
            name: SharedString::from(r.name.clone()),
        })
        .collect();

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

fn publish_chat_rows(state: &AppState, backend: ArcBackend) {
    let t0 = std::time::Instant::now();
    eprintln!("[chat] publish_chat_rows: enter");
    let snapshot = backend.blocking_read().clone();
    eprintln!("[chat] publish_chat_rows: +{}ms snapshot chats={}",
        t0.elapsed().as_millis(),
        snapshot.chats.len());

    {
        let pool = POOL.lock().unwrap().clone();
        let me_peer = CLIENT.lock().unwrap().as_ref().map(|c| c.peer_base58());
        // friend peer id -> real users.id, so the reload join shows the name.
        let mut friend_by_peer: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for c in &snapshot.contacts {
            if !c.peer_id.is_empty() {
                friend_by_peer.insert(c.peer_id.clone(), c.user_id);
            }
        }
        let dms: Vec<(String, i64)> = snapshot
            .chats
            .iter()
            .filter(|c| !c.is_group)
            .map(|c| (c.chat_id.clone(), c.time_ms))
            .collect();
        if let (Some(pool), Some(me_peer)) = (pool, me_peer) {
            if !dms.is_empty() {
                let rt = runtime();
                rt.spawn(async move {
                    for (chat_id, time_ms) in dms {
                        let Some(pb) = chat_id
                            .split('|')
                            .find(|p| !p.is_empty() && *p != me_peer)
                            .map(|s| s.to_string())
                        else {
                            continue;
                        };
                        let conv_id = match sqlite::conversations::ensure_dm(&pool, &chat_id).await {
                            Ok(id) => id,
                            Err(e) => {
                                eprintln!("[chat] ensure_dm({chat_id}) failed: {e}");
                                continue;
                            }
                        };
                        match friend_by_peer.get(&pb).copied() {
                            Some(fuid) => {
                                let exists = sqlite::devices::get_by_peer(&pool, &pb)
                                    .await
                                    .ok()
                                    .flatten()
                                    .is_some();
                                if !exists {
                                    let did = sqlite::new_id();
                                    let _ = sqlite::devices::upsert(
                                        &pool,
                                        did,
                                        &sqlite::devices::DevicePatch {
                                            user_id: Some(fuid),
                                            peer_id: Some(pb.clone()),
                                            public_key: Some(String::new()),
                                            ..Default::default()
                                        },
                                    )
                                    .await;
                                }
                            }
                            None => {
                                let _ = sqlite::devices::ensure_user_by_peer(&pool, &pb).await;
                            }
                        }
                        let _ = sqlite::conversations::set_peer_id(&pool, conv_id, &pb).await;
                        // Give a just-opened DM a time for ordering/label; only
                        // fills it if no real message time has been recorded yet.
                        if time_ms > 0 {
                            let _ = sqlite::conversations::ensure_time(&pool, conv_id, time_ms).await;
                        }
                    }
                });
            }
        }
    }

    let mut chats_sorted = snapshot.chats.clone();
    chats_sorted.sort_by(|a, b| b.time_ms.cmp(&a.time_ms).then(b.key.cmp(&a.key)));
    let chats: Vec<ConversationRow> = chats_sorted
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
    eprintln!("[chat] publish_chat_rows: +{}ms done (chats={})",
        t0.elapsed().as_millis(), snapshot.chats.len());
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
    // We must run on a thread OUTSIDE the tokio runtime: below we call
    // `backend.blocking_read()` on a `tokio::sync::RwLock`, which panics if the
    // current thread is a runtime worker. When called from an async handler
    // (e.g. the send path) we're on a worker — re-dispatch to the UI event-loop
    // thread, which is safe and also keeps UI state mutation on the main thread.
    if tokio::runtime::Handle::try_current().is_ok() {
        let weak = ui_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            load_chat_messages(weak, chat_key);
        });
        return;
    }
    let client: Option<Arc<Client<HttpDirectory>>> = CLIENT.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(client), Some(pool), Some(backend)) = (client, pool, backend) else {
        return;
    };
    let chat_id: Option<String> = {
        let g = backend.blocking_read();
        g.chat_id_for(chat_key).map(|s| s.to_string())
    };
    let Some(chat_id) = chat_id else {
        return;
    };
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
        // Order for display: newest first, regardless of the order the store
        // returns rows in. Sort by message time (then id) so the latest
        // message always renders at the top of the list.
        let mut rows = rows;
        rows.sort_by(|a, b| b.t.cmp(&a.t).then(b.id.cmp(&a.id)));
        let rendered: Vec<RenderedMsg> = {
            let mut r = RENDERER.get().unwrap().lock().unwrap();
            rows.iter().map(|m| {
                let is_self = m.sender == me_peer || m.sender == me;
                let time = time_label(m.t as i64);
                let sender = if is_self { "我" } else { &title };
                let mut h = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&m.text, &mut h);
                std::hash::Hash::hash(sender, &mut h);
                std::hash::Hash::hash(&is_self, &mut h);
                std::hash::Hash::hash(&time, &mut h);
                render_msg(&mut r, std::hash::Hasher::finish(&h), sender, &m.text, is_self, &time, 380, 2.0)
            }).collect()
        };
        let dist: f32 = {
            CHAT_SCROLL_POS.read().unwrap().get(&chat_key).copied().unwrap_or(0.0)
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
                // Restore (or bottom-align for a fresh chat) the scroll position.
                // Bumping the token lets the view re-apply `scroll-dist` on entry.
                cs.set_scroll_dist(dist);
                cs.set_scroll_token(cs.get_scroll_token() + 1);
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
    if !require_approved(weak.clone()) {
        return;
    }
    let client = CLIENT.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
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
                    // Sending a message should land us on the newest one.
                    CHAT_SCROLL_POS.write().unwrap().insert(key, 0.0);
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
    let me_peers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        if let Some(p) = client.other_peer_of(&chat_id) {
            v.push(p);
        }
        let me = client.peer_base58();
        for part in chat_id.split('|') {
            if !part.is_empty() && part != me && !v.iter().any(|x| x == part) {
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
    let my_peer = client.peer_base58();
    rt.spawn(async move {
        let mut last_err = String::new();
        let mut sent = false;
        'outer: for cand in &me_peers {
            // Skip self (defensive; extraction above already excludes it).
            if cand == &my_peer {
                continue;
            }
            match client.send_dm(cand, &body).await {
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
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend, pool) else {
        return;
    };
    let (post_id, want_liked) = {
        let mut g = backend.blocking_write();
        let (liked, _likes) = g.toggle_like(key);
        (g.post_id_for(key), liked)
    };
    let me = weak.clone().upgrade()
        .map(|ui| ui.global::<AppState>().get_user_id().to_string())
        .unwrap_or_default();
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

fn publish_my_notes(state: &AppState, backend: &ArcBackend) {
    let notes = backend.blocking_read().notes.clone();
    let w = state.get_my_notes_w().max(1);
    let (rows, content_h) = layout_my_notes(&notes, w, state.get_is_mobile());
    state.set_my_notes(slint::ModelRc::new(slint::VecModel::from(rows)));
    state.set_my_notes_content_h(content_h);
}

fn relayout_notes(weak: slint::Weak<MainWindow>, w: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let Some(backend) = backend else { return; };
    if let Some(ui) = weak.upgrade() {
        let state = ui.global::<AppState>();
        state.set_my_notes_w(w.max(1));
        publish_my_notes(&state, &backend);
    }
}

fn refresh_my_notes(weak: slint::Weak<MainWindow>) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let Some(backend) = backend else { return; };
    let me = weak.clone().upgrade()
        .map(|ui| ui.global::<AppState>().get_user_id().to_string())
        .unwrap_or_default();
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
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else { return; };
    let me = weak.clone().upgrade()
        .map(|ui| ui.global::<AppState>().get_user_id().to_string())
        .unwrap_or_default();
    if me.is_empty() || content.trim().is_empty() {
        return;
    }
    let rt = runtime();
    rt.spawn(async move {
        let me_id = match sqlite::users::ensure_identity(&pool, &me).await {
            Ok(id) => id,
            Err(_) => {
                show_app_error(weak.clone(), String::from("笔记保存失败"));
                return;
            }
        };
        let created = match sqlite::notes::create_note(&pool, me_id, &content).await {
            Ok(n) => n,
            Err(_) => {
                show_app_error(weak.clone(), String::from("笔记保存失败"));
                return;
            }
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
            show_app_notice(weak, String::from("笔记保存完成"), true);
        });
    });
}

/// Delete a note by its stable UI key: remove from backend + SQLite, publish.
fn delete_my_note(weak: slint::Weak<MainWindow>, key: i32) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
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

fn create_group_flow(weak: slint::Weak<MainWindow>, picked: Vec<i32>) -> bool {
    if !require_approved(weak.clone()) {
        return false;
    }
    eprintln!("[create_group_flow] picked={picked:?}");
    let client = CLIENT.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
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

fn search_users(weak: slint::Weak<MainWindow>, q_raw: String) {
    let q: String = q_raw.chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return;
    }

    let client = CLIENT.lock().unwrap().clone();
    match client {
        Some(client) => {
            let rt = runtime();
            let q2 = q.clone();
            rt.spawn(async move {
                // (1) Try to resolve as a username against the server directory.
                let ur = client.resolve_user(&q);
                match &ur {
                    Ok(ur) => eprintln!("[search] resolve_user OK: user_id={} e2e_public={} sign_pk={} | peer_id={} label={} endpoints={:?}",
                        ur.user.user_id, ur.user.e2e_public, ur.user.sign_pk,
                        ur.device.peer_id, ur.device.label, ur.device.endpoints),
                    Err(e) => eprintln!("[search] resolve_user ERR: {e}"),
                }
                if let Ok(ur) = ur {
                    let uname = ur.user.user_id;
                    let peer = ur.device.peer_id;
                    let row = SearchUser {
                        name: SharedString::from(uname.clone()),
                        username: SharedString::from(uname),
                        peer_id: SharedString::from(peer),
                    };
                    eprintln!("[search] -> result name={} username={} peer_id={}", row.name, row.username, row.peer_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppState>().set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![row])));
                            ui.global::<AppState>().set_add_status(SharedString::new());
                        }
                    });
                    return;
                }
                // (2) Try to resolve as a Peer ID (scan) against the server directory.
                let dev = client.dir().resolve_device(&q);
                match &dev {
                    Ok(dev) => eprintln!("[search] resolve_device OK: user_id={} peer_id={} label={} endpoints={:?} status={:?}",
                        dev.user_id, dev.peer_id, dev.label, dev.endpoints, dev.status),
                    Err(e) => eprintln!("[search] resolve_device ERR: {e}"),
                }
                if let Ok(dev) = dev {
                    let uname = dev.user_id;
                    let peer = dev.peer_id;
                    let row = SearchUser {
                        name: SharedString::from(uname.clone()),
                        username: SharedString::from(uname),
                        peer_id: SharedString::from(peer),
                    };
                    eprintln!("[search] -> result name={} username={} peer_id={}", row.name, row.username, row.peer_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<AppState>().set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![row])));
                            ui.global::<AppState>().set_add_status(SharedString::new());
                        }
                    });
                    return;
                }

                eprintln!("[search] NOT FOUND: {q2}");
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

    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let client = CLIENT.lock().unwrap().clone();
    let (Some(backend), Some(pool), Some(client)) = (backend.clone(), pool, client) else {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_add_status(SharedString::from("尚未登录或后端未就绪"));
            }
        });
        return;
    };

    let me = weak.clone().upgrade()
        .map(|ui| ui.global::<AppState>().get_user_id().to_string())
        .unwrap_or_default();

    // Self-add guard: the directory key resolves to the user's own id.
    if !me.is_empty() && username.eq_ignore_ascii_case(&me) {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_add_status(SharedString::from("不能添加自己"));
            }
        });
        return;
    }

    // Already a contact? Short-circuit before hitting sqlite.
    if backend.blocking_read().has_contact(&username, &username) {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_add_status(SharedString::from("已在通讯录中"));
            }
        });
        return;
    }

    let rt = runtime();
    rt.spawn(async move {
        if let Err(e) = do_add(&pool, &me, &username, &client, &backend).await {
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

async fn do_add(
    pool: &sqlx::SqlitePool,
    me: &str,
    username: &str,
    client: &Client<Directory>,
    backend: &ArcBackend,
) -> anyhow::Result<()> {
    let me_id = sqlite::users::ensure_identity(pool, me).await?;
    let their_id = sqlite::users::ensure_identity(pool, username).await?;
    // Backstop: refuse self-association even if the username spelling differed
    // enough to slip past the string-level guard above (e.g. leading whitespace
    // or a case-insensitive username collision).
    if their_id == me_id {
        anyhow::bail!("不能添加自己");
    }
    sqlite::social::add(pool, me_id, their_id).await?;

    // Resolve the friend's peer id from the server (directory) so we can store
    // a proper record in the local `devices` table for this contact. Best-effort:
    // if the directory lookup fails we still add the contact with the username.
    let their_peer = client
        .resolve_user(username)
        .ok()
        .map(|r| r.device.peer_id);

    if let Some(peer) = &their_peer {
        let _ = sqlite::devices::upsert(
            pool,
            sqlite::new_id(),
            &sqlite::devices::DevicePatch {
                user_id: Some(their_id),
                peer_id: Some(peer.clone()),
                public_key: Some(String::new()),
                ..Default::default()
            },
        )
        .await;
    }

    let (name, image) = {
        if let Ok(Some(u)) = sqlite::users::get(pool, their_id).await {
             (u.username.clone().unwrap_or_else(|| u.nickname.clone().unwrap_or_default()),
             u.avatar_path.clone().unwrap_or_default())
        } else {
            (username.to_string(), String::new())
        }
    };
    let mut g = backend.write().await;
    g.append_contact(their_id, their_peer.unwrap_or_else(|| username.to_string()), name, image);
    Ok(())
}

/// Build the list of this account's devices (with online/pending flags) so the
/// approval UI can render it. Called on the UI thread; reads directory (sync).
fn refresh_devices(weak: slint::Weak<MainWindow>) {
    let client = CLIENT.lock().unwrap().clone();
    let Some(client) = client else {
        return;
    };
    let me_peer = client.peer_base58();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let list: Vec<MyDevice> = client
        .my_devices()
        .into_iter()
        .map(|d| MyDevice {
            peer_id: SharedString::from(d.peer_id.clone()),
            label: SharedString::from(if d.label.is_empty() { d.peer_id.clone() } else { d.label }),
            online: now - d.seen as i64 <= chatx_core::signal::ONLINE_TTL_MS as i64,
            approved: d.status == chatx_core::account::DeviceStatus::Approved,
            self_device: d.peer_id == me_peer,
        })
        .collect();

    // Recompute whether *this* device is approved (it may have just been
    // approved from another device, or revoked).
    let approved = list
        .iter()
        .find(|d| d.peer_id.to_string() == me_peer)
        .map(|d| d.approved)
        .unwrap_or(false);
    DEVICE_APPROVED.with(|c| c.set(approved));

    // Drive the top banner text from the approval state. Kept as a distinct
    // property so a future server-published notice can override it.
    let notice = if approved {
        String::new()
    } else {
        "设备待审批 · 请让已批准设备批准后才能收发消息".to_string()
    };

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let st = ui.global::<AppState>();
            st.set_my_devices(slint::ModelRc::new(slint::VecModel::from(list)));
            st.set_device_approved(approved);
            st.set_notice_text(SharedString::from(notice));
        }
    });
}

/// Approve a pending device of this account. Requires a valid signed
/// attestation by an approved sibling; the server rejects invalid ones.
fn approve_device(weak: slint::Weak<MainWindow>, peer: String) {
    let client = CLIENT.lock().unwrap().clone();
    let Some(client) = client else {
        return;
    };
    if !device_is_approved() {
        require_approved(weak.clone());
        return;
    }
    match client.approve_device(&peer) {
        Ok(_) => {
            eprintln!("[approve] device {peer} approved");
            refresh_devices(weak.clone());
            show_app_notice(weak, "已批准该设备".to_string(), true);
        }
        Err(e) => {
            eprintln!("[approve] device {peer} failed: {e}");
            show_app_error(weak, format!("批准失败：{e}"));
        }
    }
}

/// Revoke an approved or already-revoked device of this account.
fn revoke_device(weak: slint::Weak<MainWindow>, peer: String) {
    let client = CLIENT.lock().unwrap().clone();
    let Some(client) = client else {
        return;
    };
    if !device_is_approved() {
        require_approved(weak.clone());
        return;
    }
    match client.revoke_device(&peer) {
        Ok(_) => {
            eprintln!("[revoke] device {peer} revoked");
            refresh_devices(weak.clone());
            show_app_notice(weak, "已撤销该设备".to_string(), true);
        }
        Err(e) => {
            eprintln!("[revoke] device {peer} failed: {e}");
            show_app_error(weak, format!("撤销失败：{e}"));
        }
    }
}

/// Gate an outbound/privileged action on device approval. Returns `true` to
/// proceed; on a pending device it shows a notice and returns `false`.
fn require_approved(weak: slint::Weak<MainWindow>) -> bool {
    if device_is_approved() {
        return true;
    }
    show_app_error(weak, "当前设备待审批，请先在已登录设备上批准该设备".to_string());
    false
}

/// Show a transient global error dialog (transparent backdrop, message + 知道了).
fn show_app_error(weak: slint::Weak<MainWindow>, msg: String) {
    show_app_notice(weak, msg, false);
}

/// Show a transient global notice dialog; `success` picks the green checkmark style.
fn show_app_notice(weak: slint::Weak<MainWindow>, msg: String, success: bool) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<AppState>();
            state.set_error_message(SharedString::from(msg));
            state.set_error_is_success(success);
            let mut nav = state.get_nav_state();
            nav.global_overlay = GlobalOverlayType::Error;
            state.set_nav_state(nav);
        }
    });
}


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

/// Must run on the UI thread — the Slint event loop owns `call::CALL` (a
/// thread_local) and cpal's streams.
fn begin_voice_call(weak: slint::Weak<MainWindow>, key: i32) {
    if !require_approved(weak.clone()) {
        return;
    }
    eprintln!("[audio.call] begin_voice_call: enter key={key}");
    let client = CLIENT.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(client), Some(backend)) = (client, backend) else {
        eprintln!("[audio.call] begin_voice_call: CLIENT or BACKEND not ready — abort");
        publish_call_state(weak.clone(), None, None, false);
        show_app_error(weak, String::from("尚未登录或后端未就绪，无法发起通话"));
        return;
    };
    let client = Arc::new(client);

    let (peer_base58, peer_name) = {
        let g = backend.blocking_read();
        // `peer_id_for` returns the contact's base-58 peer id (what we use as
        // the directory username to resolve).
        let Some(pid) = g.peer_id_for(key).map(|s| s.to_string()) else {
            eprintln!("[audio.call] begin_voice_call: no peer_id for key={key} — abort");
            show_app_error(weak, format!("联系人已失效（key={key}），无法发起通话"));
            return;
        };
        let name = g
            .contacts
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| pid.clone());
        eprintln!(
            "[audio.call] begin_voice_call: resolving peer_base58={pid} name={name}"
        );
        (pid, name)
    };

    eprintln!("[audio.call] begin_voice_call: about to call::start_call");
    match call::start_call(
        Arc::clone(&client),
        &peer_base58,
        call::CallOptions { include_cam: false, include_scr: false },
    ) {
        Ok(()) => {
            eprintln!("[audio.call] begin_voice_call: start_call OK — showing AudioCall overlay");
            publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
            show_call_overlay(weak, GlobalOverlayType::AudioCall);
        }
        Err(e) => {
            eprintln!("[audio.call] begin_voice_call: start_call FAILED: {e}");
            show_app_error(weak, format!("通话开始失败: {e}"));
        }
    }
}

fn begin_video_call(weak: slint::Weak<MainWindow>, key: i32) {
    if !require_approved(weak.clone()) {
        return;
    }
    let client = CLIENT.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(client), Some(backend)) = (client, backend) else {
        publish_call_state(weak, None, None, false);
        return;
    };
    let client = Arc::new(client);

    let (peer_base58, peer_name) = {
        let g = backend.blocking_read();
        let Some(pid) = g.peer_id_for(key).map(|s| s.to_string()) else {
            return;
        };
        let name = g.contacts.iter().find(|r| r.key == key).map(|r| r.name.clone()).unwrap_or_else(|| pid.clone());
        (pid, name)
    };

    CAMERA.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(camera::Camera::new());
        }
        let mut guard = slot.borrow_mut();
        let cam = guard.as_mut().expect("camera inited");
        if !cam.is_running() {
            cam.set_frame_sink(call_dispatcher::on_camera_frame);
            if let Err(e) = cam.start() {
                eprintln!("[video.call] cam.start: {e}");
            }
        }
    });

    if let Err(e) = call::start_call(
        Arc::clone(&client),
        &peer_base58,
        call::CallOptions { include_cam: true, include_scr: false },
    ) {
        publish_call_error(weak, format!("视频通话开始失败: {e}"));
        return;
    }
    publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
    show_call_overlay(weak, GlobalOverlayType::VideoCall);
}

fn publish_call_error(weak: slint::Weak<MainWindow>, msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let state = ui.global::<CallState>();
            state.set_error_message(SharedString::from(msg));
        }
    });
}

fn end_voice_call(weak: slint::Weak<MainWindow>) {
    let had_active = call::is_active();
    call::stop_call();
    if had_active {
        publish_call_state(weak.clone(), None, None, false);
        show_call_overlay(weak, GlobalOverlayType::None);
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn screen_sink_adapter(bytes: Vec<u8>, w: u32, h: u32) {
    call_dispatcher::on_screen_frame_rgba(bytes, w, h);
}

fn begin_screen_share(weak: slint::Weak<MainWindow>, key: i32) {
    if !require_approved(weak.clone()) {
        return;
    }
    let client = CLIENT.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(client), Some(backend)) = (client, backend) else {
        return;
    };
    let client = Arc::new(client);
    let (peer_base58, peer_name) = {
        let g = backend.blocking_read();
        let Some(pid) = g.peer_id_for(key).map(|s| s.to_string()) else {
            return;
        };
        let name = g.contacts.iter().find(|r| r.key == key).map(|r| r.name.clone()).unwrap_or_else(|| pid.clone());
        (pid, name)
    };

    // Desktop: ensure the screen capture is running.
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
        eprintln!("[screen] requesting MediaProjection from Java shell");
        bridge::request_screen_share_start();
    }
    #[cfg(target_os = "ios")]
    {
        eprintln!("[screen] requesting RPScreenRecorder from ObjC shim");
        bridge::request_screen_share_start();
    }

    if let Err(e) = call::start_call(
        Arc::clone(&client),
        &peer_base58,
        call::CallOptions { include_cam: false, include_scr: true },
    ) {
        publish_call_error(weak, format!("屏幕共享开始失败: {e}"));
        return;
    }
    publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
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
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
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
        let (avatar_path, nickname, username, bio, created_at, moment) = {
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
            let avatar = load_image_cached(&avatar_path);
            let (has_moment, mt, mtime, ml) = match moment {
                Some((text, ts, likes)) => (true, text, ts, likes),
                None => (false, String::new(), 0, 0),
            };
            ps.set_key(my_key);
            ps.set_avatar(avatar);
            ps.set_nickname(SharedString::from(nickname));
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
    let t0 = std::time::Instant::now();
    eprintln!("[chat] open_conversation_with_contact: enter key={key}");
    let client = CLIENT.lock().unwrap().clone();
    eprintln!("[chat] open_conversation: +{}ms grabbed CLIENT", t0.elapsed().as_millis());
    let backend = BACKEND.lock().unwrap().clone();
    eprintln!("[chat] open_conversation: +{}ms grabbed BACKEND", t0.elapsed().as_millis());
    let (Some(client), Some(backend)) = (client, backend) else {
        show_app_error(weak, format!("尚未登录或后端未就绪"));
        return;
    };
    let row_clone = {
        let g = backend.blocking_read();
        let row = g.contacts.iter().find(|r| r.key == key).cloned();
        drop(g);
        row
    };
    eprintln!("[chat] open_conversation: +{}ms backend row_clone ok={}", t0.elapsed().as_millis(), row_clone.is_some());
    let Some(c) = row_clone else {
        show_app_error(weak, format!("联系人已失效"));
        return;
    };
    if c.peer_id.is_empty() {
        show_app_error(weak, format!("联系人缺少 peer id，无法发起会话"));
        return;
    }
    let peer_id = c.peer_id;
    let title = c.name;
    eprintln!("[chat] open_conversation: +{}ms peer_id={peer_id}", t0.elapsed().as_millis());
    let me_peer = client.peer_base58();
    eprintln!("[chat] open_conversation: +{}ms self peer={me_peer}", t0.elapsed().as_millis());
    let chat_id = dm_chat_id(&me_peer, &peer_id);
    eprintln!("[chat] open_conversation: +{}ms chat_id={chat_id}", t0.elapsed().as_millis());
    eprintln!("[chat] open_conversation: +{}ms THREAD {:?} about to acquire READ lock (2nd time, after row_clone)",
        t0.elapsed().as_millis(), std::thread::current().id());
    let existing_key = backend.blocking_read().chats.iter().find(|c| c.chat_id == chat_id).map(|c| c.key);
    eprintln!("[chat] open_conversation: +{}ms THREAD {:?} READ lock released, existing={:?}",
        t0.elapsed().as_millis(), std::thread::current().id(), existing_key);
    let new_key = match existing_key {
        Some(k) => k,
        None => {
            eprintln!("[chat] open_conversation: +{}ms THREAD {:?} about to acquire WRITE lock (append_chat)",
                t0.elapsed().as_millis(), std::thread::current().id());
            let mut g = backend.blocking_write();
            let k = g.append_chat(chat_id, title, false);
            drop(g);
            eprintln!("[chat] open_conversation: +{}ms THREAD {:?} WRITE lock released, appended new chat key={k}",
                t0.elapsed().as_millis(), std::thread::current().id());
            k
        }
    };
    eprintln!("[chat] open_conversation: +{}ms new_key={new_key}, about to invoke_from_event_loop", t0.elapsed().as_millis());
    let _ = slint::invoke_from_event_loop(move || {
        eprintln!("[chat] open_conversation: +{}ms eventloop cb enter", t0.elapsed().as_millis());
        let Some(ui) = weak.upgrade() else {
            eprintln!("[chat] open_conversation: +{}ms weak upgrade failed", t0.elapsed().as_millis());
            return;
        };
        eprintln!("[chat] open_conversation: eventloop cb calling publish_chat_rows");
        let state = ui.global::<AppState>();
        publish_chat_rows(&state, backend.clone());
        eprintln!("[chat] open_conversation: eventloop cb publish_chat_rows done, calling load_chat_messages");
        load_chat_messages(ui.as_weak(), new_key);
        eprintln!("[chat] open_conversation: eventloop cb load_chat_messages spawned, calling push_sub_history");
        // Ensure we're on the Chat tab (first tab) so the pushed ChatRoom
        // subpage lands under the visible tab, regardless of which tab the
        // user was on when they opened the conversation from a profile.
        {
            let mut nav = state.get_nav_state();
            if nav.active_tab != 0 {
                nav.active_tab = 0;
                state.set_nav_state(nav);
            }
        }
        push_sub_history(&state, SubPageEntry { page: SubPageType::ChatRoom, payload: new_key });
        eprintln!("[chat] open_conversation: +{}ms eventloop cb done", t0.elapsed().as_millis());
    });
    eprintln!("[chat] open_conversation: +{}ms invoke_from_event_loop queued (not yet run)", t0.elapsed().as_millis());
}

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

fn parse_media_urls(s: &str) -> Vec<String> {
    let t = s.trim();
    if t.is_empty() {
        return Vec::new();
    }
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

fn load_post_detail(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let Some(post_id) = detail_key_post(&backend, key) else {
        publish_detail_status(weak, key, SharedString::from("帖子已失效"));
        return;
    };

    let me = weak.clone().upgrade()
        .map(|ui| ui.global::<AppState>().get_user_id().to_string())
        .unwrap_or_default();
    let author = {
        let g = backend.blocking_read();
        g.discover
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.author.clone())
            .unwrap_or_default()
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
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
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
        let me = weak.clone().upgrade()
            .map(|ui| ui.global::<AppState>().get_user_id().to_string())
            .unwrap_or_default();
        rt.spawn(async move {
            let _ = data::persist_like_toggle(&pool, &me, post_id, want_liked).await;
        });
    }
}

/// Add a comment to the post shown in the detail view: persist to SQLite,
/// prepend it to the comment list, and bump the comment count.
fn add_post_comment(weak: slint::Weak<MainWindow>, key: i32, body: String) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(weak, key, SharedString::from("尚未登录或后端未就绪"));
        return;
    };
    let Some(post_id) = detail_key_post(&backend, key) else {
        publish_detail_status(weak, key, SharedString::from("帖子已失效"));
        return;
    };
    let me = weak.clone().upgrade()
        .map(|ui| ui.global::<AppState>().get_user_id().to_string())
        .unwrap_or_default();
    let my_name = {
        let n = weak.clone().upgrade()
            .map(|ui| ui.global::<AppState>().get_nickname().to_string())
            .unwrap_or_default();
        if n.is_empty() { me.clone() } else { n }
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
