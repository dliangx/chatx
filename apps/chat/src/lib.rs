use slint::{Image as SlintImage, SharedString};
use std::cell::RefCell;
use std::sync::Arc;

use chatx_core::Client;
use chatx_core::friendq::{FriendQ, FriendQAction};
use chatx_core::group::MemberInfo;
use chatx_core::signal::{DirectoryClient, HttpDirectory};
use chatx_core::store::dm_chat_id;
use render::bubble::Side;

pub mod data;
use data::ArcBackend;
pub mod call;
pub mod call_dispatcher;
pub mod image;
pub mod moment;
pub mod process;
pub mod time;
use image::{
    ChatMsg, RENDERER, RenderedMsg, ensure_renderer, load_image_cached, make_avatar,
    make_preview_image, qr_image, render_msg, to_message_data,
};
use process::{group_conn_enter, start_group_conn_sweeper, touch_chat_activity};
use time::{fmt_profile_date, fmt_time, time_label};

use camera::Camera;
use screen::Screen;

slint::include_modules!();

type Directory = HttpDirectory;

pub(crate) static CLIENT: std::sync::Mutex<Option<Arc<Client<Directory>>>> =
    std::sync::Mutex::new(None);
static POOL: std::sync::Mutex<Option<Arc<sqlx::SqlitePool>>> = std::sync::Mutex::new(None);
static BACKEND: std::sync::Mutex<Option<ArcBackend>> = std::sync::Mutex::new(None);

thread_local! {
    static RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> = RefCell::new(None);
    static PUMP_STARTED: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static CAMERA: RefCell<Option<Camera>> = RefCell::new(None);
    static SCREEN: RefCell<Option<Screen>> = RefCell::new(None);
    static DEVICE_APPROVED: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// Set once a scanned QR is resolved, so late frames don't re-fire it.
static SCAN_RESOLVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ordered on-disk photo paths selected for the share / publish draft.
/// The UI thumbnails (`MomentState.media`) are kept in sync from this list:
/// `add` / `remove` / `publish` all read & mutate it, then republish the
/// `[image]` model. The single source of truth so `publish` can build the
/// `media_urls` JSON array to persist.
static MOMENT_MEDIA: std::sync::LazyLock<std::sync::Mutex<Vec<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

fn device_is_approved() -> bool {
    DEVICE_APPROVED.with(|c| c.get())
}

static CHAT_SCROLL_POS: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<i32, f32>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

static CURRENT_ME: std::sync::LazyLock<std::sync::RwLock<String>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(String::new()));

fn set_current_me(user_id: &str) {
    *CURRENT_ME.write().unwrap() = user_id.to_string();
}

fn current_me() -> String {
    CURRENT_ME.read().unwrap().clone()
}

/// Read the persisted light/dark theme (stored in the `settings` table) before
/// login. `apply_result` — where login side-effects kick in — runs after the
/// auth screen is already visible, so this restores the saved scheme early
/// enough for the auth screen to render in it. The DB lives at
/// `~/.config/chatx/<profile>/messages.db`. `None` means "no DB / no value".
async fn load_persisted_theme() -> Option<bool> {
    let base = chatx_core::identity::DeviceIdentity::home_dir();
    let profile = profile().trim().to_lowercase();
    let dir = if profile.is_empty() {
        base.clone()
    } else {
        base.join(profile)
    };
    let path = dir.join("messages.db");
    if !path.exists() {
        return None;
    }
    let url = format!("sqlite:{}?mode=ro", path.display());
    let Ok(pool) = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return None;
    };
    let v = sqlite::settings::get(&pool, "theme").await.ok().flatten()?;
    Some(v == "dark")
}

static CURRENT_CHAT: std::sync::LazyLock<std::sync::RwLock<Option<String>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(None));

fn set_current_chat_id(chat_id: &str) {
    *CURRENT_CHAT.write().unwrap() = Some(chat_id.to_string());
}
static PROFILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PROFILE_LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static POST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

static POST_LATEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn runtime() -> Arc<tokio::runtime::Runtime> {
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
    #[cfg(any(target_os = "android", target_os = "ios"))]
    install_bridge_sinks();

    let ui = MainWindow::new().expect("window init failed");
    let state = ui.global::<AppState>();
    let weak = ui.as_weak();

    // Default language is English. Bundled translations become active once a
    // component exists (created above), so we can select the initial language now.
    let _ = slint::select_bundled_translation(&state.get_lang());

    // Restore the persisted light/dark theme before the login block runs.
    // `apply_result` (called from the login flow below) is too late — the
    // auth screen is already visible by then. Reading is async so we spawn a
    // task; the UI call hops onto the event-loop thread via `invoke_from_event_loop`.
    {
        let w2 = weak.clone();
        runtime().spawn(async move {
            let Some(dark) = load_persisted_theme().await else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w2.upgrade() {
                    ui.global::<Theme>().set_dark(dark);
                }
                if let Some(r) = RENDERER.get() {
                    r.lock().unwrap().set_theme(if dark {
                        render::theme::Theme::dark()
                    } else {
                        render::theme::Theme::default()
                    });
                }
            });
        });
    }

    state.set_user_id(SharedString::from(""));
    state.set_is_mobile(cfg!(target_os = "android") || cfg!(target_os = "ios"));
    // state.set_is_mobile(true);
    let existing_uid = chatx_core::account::Keystore::load(&keystore_path(&profile()))
        .map(|ks| ks.user_id)
        .ok();
    if let Some(uid) = existing_uid {
        ui.global::<AppState>()
            .set_auth_message(SharedString::from("please logging in "));
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
        let tabs: Vec<TabState> = (0..4)
            .map(|_| TabState {
                sub_history: slint::ModelRc::new(slint::VecModel::from(Vec::<SubPageEntry>::new())),
                sub_top: -1,
            })
            .collect();
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
                // Keep `current-chat-peer-key` in sync so the chat add-menu's
                // Voice / video call and screen share can route through the same
                // ProfileState callbacks the profile page uses.
                let k = chat_key_to_contact_key(payload);
                if let Some(ui) = w.upgrade() {
                    let s = ui.global::<AppState>();
                    s.set_current_chat_peer_key(k);
                }
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
                if let Some(popped) = pop_sub_history(&ui.global::<AppState>()) {
                    release_popped_page(&ui, popped.page);
                }
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

    {
        let w = weak.clone();
        ui.global::<AppState>().on_tab_changed(move |tab: i32| {
            // "Discover" (tab index 2) — kick off the moment sync round.
            if tab == 2 {
                moment::trigger_round(w.clone());
            }
        });
    }

    let w = weak.clone();
    ui.global::<AppState>().on_logout(move || {
        *CLIENT.lock().unwrap() = None;
        *POOL.lock().unwrap() = None;
        *BACKEND.lock().unwrap() = None;
        set_current_me("");
        PUMP_STARTED.set(false);
        DEVICE_APPROVED.with(|c| c.set(false));
        if let Some(ui) = w.upgrade() {
            let st = ui.global::<AppState>();
            let uid = st.get_user_id().to_string();
            if !uid.is_empty() {
                clear_passphrase(&uid);
            }
            clear_sub_history(&st);
            st.set_chats(slint::ModelRc::new(slint::VecModel::from(Vec::<
                ConversationRow,
            >::new(
            ))));
            st.set_contacts(slint::ModelRc::new(slint::VecModel::from(
                Vec::<ContactRow>::new(),
            )));
            st.set_contact_letters(slint::ModelRc::new(slint::VecModel::from(
                Vec::<LetterEntry>::new(),
            )));
            st.set_discover(slint::ModelRc::new(slint::VecModel::from(Vec::<
                DiscoverCard,
            >::new(
            ))));
            st.set_my_devices(slint::ModelRc::new(slint::VecModel::from(
                Vec::<MyDevice>::new(),
            )));
            st.set_device_approved(true);
            st.set_notice_text(SharedString::new());
            st.set_data_status(SharedString::new());
            let cs = ui.global::<ChatSession>();
            cs.set_messages(slint::ModelRc::new(slint::VecModel::from(
                Vec::<MessageData>::new(),
            )));
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
        ui.global::<AppState>().on_discover_load_more(move || {
            discover_load_more(weak.clone());
        });
    }

    ui.global::<AppState>().on_select_language(move |lang| {
        let lang = lang.to_string();
        // Switch the bundled translation, then force every component to re-render
        // so reactive bindings pick up the newly selected catalog.
        if lang.is_empty() || lang == "en" {
            let _ = slint::select_bundled_translation("");
        } else {
            let _ = slint::select_bundled_translation(&lang);
        }
        slint::update_all_translations();
        // Persist for the next launch (read back from the `settings` table on startup).
        let pool = POOL.lock().unwrap().clone();
        if let Some(pool) = pool {
            let lang = lang.clone();
            runtime().spawn(async move {
                let _ = sqlite::settings::set(&pool, "lang", &lang).await;
            });
        }
    });

    {
        let weak = weak.clone();
        ui.global::<AppState>().on_toggle_theme(move |dark| {
            // 1) Flip the Slint theme global — every `Theme.*` in the UI is a
            //    computed brush tied to `Theme.dark`, so the whole tree re-tints
            //    reactively.
            if let Some(ui) = weak.upgrade() {
                ui.global::<Theme>().set_dark(dark);
            }

            // 2) Update the Rust-side chat renderer so future bubble draws use
            //    the new palette (theme_version bump also invalidates the cache).
            {
                let theme = if dark {
                    render::theme::Theme::dark()
                } else {
                    render::theme::Theme::default()
                };
                if let Some(r) = RENDERER.get() {
                    r.lock().unwrap().set_theme(theme);
                }
            }

            // 3) Re-render the currently-viewed chat (if any) so the already-
            //    baked bubble images in the list pick up the new palette without
            //    the user having to leave and re-enter the chat.
            if let Some(ui) = weak.upgrade() {
                let active = ui.global::<AppState>().get_current_chat_peer_key();
                if active >= 0 {
                    load_chat_messages(ui.as_weak(), active);
                }
            }

            // 4) Persist for the next launch (read back from the `settings` table
            //    on startup, mirroring how `lang` is handled).
            let pool = POOL.lock().unwrap().clone();
            if let Some(pool) = pool {
                runtime().spawn(async move {
                    let v = if dark { "dark" } else { "light" };
                    let _ = sqlite::settings::set(&pool, "theme", v).await;
                });
            }
        });
    }

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
        ui.global::<AppState>().on_copy_note(move |text| {
            copy_note_to_clipboard(text.to_string());
        });
    }
    {
        let weak = weak.clone();
        ui.global::<AppState>().on_delete_contact(move |id| {
            delete_contact(weak.clone(), id);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<AppState>().on_delete_chat(move |id| {
            delete_chat(weak.clone(), id);
        });
    }
    {
        use slint::Model;
        let w = weak.clone();
        ui.global::<AppState>().on_pick_group_member(move |key| {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let m = st.get_group_picked();
                let mut picked: Vec<i32> =
                    (0..m.row_count()).filter_map(|i| m.row_data(i)).collect();
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
                let mut picked: Vec<i32> =
                    (0..m.row_count()).filter_map(|i| m.row_data(i)).collect();
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
                    st.set_group_picked(slint::ModelRc::new(slint::VecModel::from(
                        Vec::<i32>::new(),
                    )));
                }
            }
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>()
            .on_show_qr_code(move |peer: SharedString| {
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
        ui.global::<AppState>().on_open_image_viewer(move || {
            if let Some(ui) = w.upgrade() {
                let st = ui.global::<AppState>();
                let mut nav = st.get_nav_state();
                nav.global_overlay = GlobalOverlayType::ImageViewer;
                st.set_nav_state(nav);
            }
        });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>().on_close_image_viewer(move || {
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
        ui.global::<AppState>()
            .on_add_contact(move |username, peer_id| {
                add_contact(w.clone(), username.to_string(), peer_id.to_string());
            });
    }

    {
        let w = weak.clone();
        ui.global::<AppState>()
            .on_accept_friend_request(move |key| {
                accept_friend_request(w.clone(), key);
            });
    }
    {
        let w = weak.clone();
        ui.global::<AppState>()
            .on_reject_friend_request(move |key| {
                reject_friend_request(w.clone(), key);
            });
    }

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

    {
        let weak = weak.clone();
        ui.global::<ProfileState>().on_request_load(move |key| {
            load_profile_data(weak.clone(), key);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<ProfileState>()
            .on_open_conversation(move |key| {
                eprintln!("[chat] on_open_conversation: got key={key}");
                open_conversation_with_contact(weak.clone(), key);
            });
    }
    call::set_weak_window(weak.clone());
    // Observe the *peer's* camera / screen on-off control signals. The media
    // crate fires this hook when it receives a `SignalKind::Cam`/`Screen` on
    // an active call; we flip the matching `has-remote-*` flag so the A/B
    // surfaces + buttons update.
    {
        let wk_outer = weak.clone();
        media::set_video_signal_cb(Some(std::sync::Arc::new(move |kind, on| {
            let wk = wk_outer.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = wk.upgrade() {
                    let cs = ui.global::<CallState>();
                    match kind {
                        media::VKind::Cam => {
                            cs.set_has_remote_cam(on);
                            if !on {
                                cs.set_remote_cam(slint::Image::default());
                            }
                        }
                        media::VKind::Scr => {
                            cs.set_has_remote_scr(on);
                            if !on {
                                cs.set_remote_scr(slint::Image::default());
                            }
                        }
                    }
                }
            });
        })));
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
        ui.global::<ProfileState>()
            .on_start_screen_share(move |key| {
                begin_screen_share(weak.clone(), key);
            });
    }
    {
        let weak = weak.clone();
        ui.global::<AppState>().on_stop_screen_share(move || {
            end_screen_share(weak.clone());
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
        ui.global::<CallState>()
            .on_toggle_mute(move || match weak.upgrade() {
                Some(ui) => {
                    let st = ui.global::<CallState>();
                    let new = !st.get_muted();
                    st.set_muted(new);
                    call::mute_mic(new);
                    if let Some(c) = call::active_call() {
                        c.send_control(media::SignalKind::MuteMic(new));
                    }
                }
                None => {}
            });
    }
    {
        let weak = weak.clone();
        ui.global::<CallState>()
            .on_toggle_audio(move || match weak.upgrade() {
                Some(ui) => {
                    let st = ui.global::<CallState>();
                    let new = !st.get_audio_muted();
                    st.set_audio_muted(new);
                    call::set_remote_mute(new);
                }
                None => {}
            });
    }
    {
        let weak = weak.clone();
        ui.global::<CallState>().on_toggle_cam(move || {
            toggle_my_video(weak.clone(), media::VKind::Cam);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<CallState>().on_toggle_scr(move || {
            toggle_my_video(weak.clone(), media::VKind::Scr);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<CallState>()
            .on_swap_ab(move || match weak.upgrade() {
                Some(ui) => {
                    let st = ui.global::<CallState>();
                    st.set_swapped(!st.get_swapped());
                }
                None => {}
            });
    }
    {
        let weak = weak.clone();
        ui.global::<CallState>()
            .on_toggle_minimize(move || match weak.upgrade() {
                Some(ui) => {
                    let st = ui.global::<CallState>();
                    st.set_minimized(!st.get_minimized());
                }
                None => {}
            });
    }

    {
        let weak = weak.clone();
        ui.global::<ChatSession>()
            .on_message_sent(move |key, body| {
                let b = body.as_str().trim().to_string();
                if b.is_empty() {
                    return;
                }
                send_chat_message(weak.clone(), key, b);
            });
    }

    {
        ui.global::<ChatSession>()
            .on_update_scroll(move |key, dist| {
                CHAT_SCROLL_POS.write().unwrap().insert(key, dist);
            });
    }

    {
        let weak = weak.clone();
        ui.global::<ChatSession>().on_scroll_request(move |key| {
            let dist = CHAT_SCROLL_POS
                .read()
                .unwrap()
                .get(&key)
                .copied()
                .unwrap_or(0.0);
            if let Some(ui) = weak.upgrade() {
                let cs = ui.global::<ChatSession>();
                cs.set_scroll_dist(dist);
                cs.set_scroll_token(cs.get_scroll_token() + 1);
            }
        });
    }

    {
        let weak = weak.clone();
        ui.global::<PostDetailState>().on_request_load(move |key| {
            // Release the previous post's full-resolution images before we go
            // off to load the new one; holding both at once doubles the bitmap
            // memory in a brief window and pins the old buffer past the point
            // the user has moved on.
            if let Some(ui) = weak.upgrade() {
                let empty: Vec<SlintImage> = Vec::new();
                ui.global::<PostDetailState>()
                    .set_media(slint::ModelRc::new(slint::VecModel::from(empty)));
            }
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
        ui.global::<PostDetailState>()
            .on_add_comment(move |key, body| {
                let b = body.as_str().trim().to_string();
                if b.is_empty() {
                    return;
                }
                add_post_comment(weak.clone(), key, b);
            });
    }
    // ── install the album-photo sink (shared with all picker callbacks) ────
    {
        camera::album::set_photos_sink(move |paths: Vec<String>| {
            on_moment_photos_selected(paths);
        });
    }

    // ── Share-now camera overlay (start / capture / close) ────────────────
    {
        let weak = weak.clone();
        ui.global::<CameraState>().on_start(move || {
            start_camera_capturer(weak.clone());
        });
    }
    {
        let weak = weak.clone();
        ui.global::<CameraState>().on_capture(move || {
            trigger_photo_capture(weak.clone());
        });
    }
    {
        let weak = weak.clone();
        ui.global::<CameraState>().on_close(move || {
            stop_camera_capturer(weak.clone());
        });
    }
    {
        let weak = weak.clone();
        ui.global::<CameraState>().on_finish(move || {
            finish_camera_and_open_publish(weak.clone());
        });
    }

    // ── Publish page (pick / remove / publish) ─────────────────────────────
    {
        let weak = weak.clone();
        ui.global::<MomentState>().on_pick_photos(move || {
            let n = {
                let Some(ui) = weak.upgrade() else { return };
                let m = ui.global::<MomentState>().get_media();
                use slint::Model;
                m.row_count()
            };
            if n >= 9 {
                eprintln!("[moment] photo cap reached (9); ignoring pick");
                return;
            }
            if let Err(e) = camera::album::pick_photos(9) {
                eprintln!("[moment] pick photos failed: {e}");
                if let Some(ui) = weak.upgrade() {
                    let st = ui.global::<MomentState>();
                    st.set_status(SharedString::from(format!("Failed to pick image: {e}")));
                }
            }
        });
    }
    {
        ui.global::<MomentState>().on_remove_photo(move |i| {
            moment_remove_photo(i as i32);
        });
    }
    {
        let weak = weak.clone();
        ui.global::<MomentState>().on_publish(move || {
            publish_moment(weak.clone());
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
            SCAN_RESOLVED.store(false, std::sync::atomic::Ordering::SeqCst);
            state.set_scan_found(false);
            state.set_scan_preview(slint::Image::default());
            state.set_scan_status(SharedString::from("Camera on. Point it at the QR code…"));
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

    bridge::set_audio_consumer(Box::new(move |bytes, rate, ch| {
        let n = AUDIO_N.fetch_add(1, Ordering::Relaxed);
        if n % 100 == 0 {
            eprintln!("[bridge] mic #{n} {}B @{}Hz/{}ch", bytes.len(), rate, ch);
        }
        call_dispatcher::on_audio_frame(bytes, rate);
    }));
    bridge::set_screen_consumer(Box::new(move |bytes, w, h, fmt| {
        call_dispatcher::on_screen_frame(bytes.to_vec(), w, h, fmt);
    }));
    bridge::set_camera_consumer(Box::new(move |bytes, w, h, fmt| {
        call_dispatcher::on_camera_frame(bytes, w, h, fmt);
    }));
}

fn push_sub_history(app: &AppState, entry: SubPageEntry) {
    let mut nav = app.get_nav_state();
    let active = nav.active_tab as usize;
    use slint::Model;
    let tabs_model = nav.tabs.clone();
    let mut v: Vec<TabState> = (0..tabs_model.row_count())
        .filter_map(|i| tabs_model.row_data(i))
        .collect();
    if active >= v.len() {
        return;
    }
    let tab = v.get_mut(active).unwrap();
    let h = tab.sub_history.clone();
    let keep = if tab.sub_top < 0 {
        0
    } else {
        tab.sub_top as usize + 1
    };
    let mut v2: Vec<SubPageEntry> = (0..h.row_count()).filter_map(|i| h.row_data(i)).collect();
    v2.truncate(keep);
    v2.push(entry);
    tab.sub_history = slint::ModelRc::new(slint::VecModel::from(v2));
    tab.sub_top = tab.sub_history.row_count() as i32 - 1;
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(v));
    app.set_nav_state(nav);
}

/// Pop the top of the current tab's subpage stack. Returns the popped entry
/// (if any) so the caller can release page-scoped resources (loaded images,
/// in-flight drafts, etc.) while still on the UI thread.
fn pop_sub_history(app: &AppState) -> Option<SubPageEntry> {
    let mut nav = app.get_nav_state();
    let active = nav.active_tab as usize;
    use slint::Model;
    let mut v: Vec<TabState> = (0..nav.tabs.row_count())
        .filter_map(|i| nav.tabs.row_data(i))
        .collect();
    if active >= v.len() {
        return None;
    }
    let tab = v.get_mut(active).unwrap();
    let popped = if tab.sub_top >= 0 {
        let h = tab.sub_history.clone();
        (0..h.row_count())
            .filter_map(|i| h.row_data(i))
            .collect::<Vec<_>>()
            .get(tab.sub_top as usize)
            .cloned()
    } else {
        None
    };
    let next_top = tab.sub_top - 1;
    if next_top < 0 {
        tab.sub_history = slint::ModelRc::new(slint::VecModel::from(Vec::<SubPageEntry>::new()));
        tab.sub_top = -1;
    } else {
        tab.sub_top = next_top;
    }
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(v));
    app.set_nav_state(nav);
    popped
}

/// Release page-scoped image buffers + draft state when the given page is
/// popped. Held otherwise, each post's full-resolution bitmaps / moment draft
/// thumbnails would stay alive until the *next* page of the same type is
/// loaded — leaving the memory pinned across tab switches, back navigations,
/// and idle time.
fn release_popped_page(ui: &MainWindow, page: SubPageType) {
    match page {
        SubPageType::DiscoverDetail => {
            let ps = ui.global::<PostDetailState>();
            let empty: Vec<SlintImage> = Vec::new();
            ps.set_media(slint::ModelRc::new(slint::VecModel::from(empty)));
            let empty: Vec<CommentRow> = Vec::new();
            ps.set_comment_list(slint::ModelRc::new(slint::VecModel::from(empty)));
            ps.set_key(-1);
            ps.set_title(SharedString::new());
            ps.set_author(SharedString::new());
            ps.set_liked_by(SharedString::new());
            ps.set_time(SharedString::new());
            ps.set_status(SharedString::new());
        }
        SubPageType::AddMoment => {
            let ms = ui.global::<MomentState>();
            let empty: Vec<SlintImage> = Vec::new();
            ms.set_media(slint::ModelRc::new(slint::VecModel::from(empty)));
            ms.set_title(SharedString::new());
            ms.set_content(SharedString::new());
            ms.set_status(SharedString::new());
            reset_moment_draft();
        }
        _ => {}
    }
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

/// Resolve a scanned QR: stop the camera, close the scan overlay, open the
/// AddContact subpage on the current tab, then run `search_users` so the
/// peer shows up in `add_contact.slint`.
fn on_scan_success(weak: slint::Weak<MainWindow>, text: String) {
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let st = ui.global::<AppState>();

        st.invoke_scanner_stop();
        st.set_scan_found(false);
        st.set_search_results(slint::ModelRc::new(slint::VecModel::from(vec![])));

        let mut nav = st.get_nav_state();
        nav.global_overlay = GlobalOverlayType::None;
        st.set_nav_state(nav);
        push_sub_history(
            &st,
            SubPageEntry {
                page: SubPageType::AddContact,
                payload: 0,
            },
        );

        search_users_by_peer_id(weak.clone(), text);
    });
}

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
                    if SCAN_RESOLVED
                        .compare_exchange(
                            false,
                            true,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        )
                        .is_err()
                    {
                        return;
                    }
                    eprintln!(
                        "[scan] resolved: \"{}\" — stopping camera + opening add contact",
                        text
                    );
                    on_scan_success(wsink.clone(), text);
                });

                let latest_seen = std::sync::atomic::AtomicU64::new(0);
                call_dispatcher::install_qr_hook(move |bytes, w, h, fmt| {
                    const GATE_MS: u64 = 30;
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
                    if SCAN_RESOLVED.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    let data = bytes.to_vec();
                    let wk = wsink2.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if SCAN_RESOLVED.load(std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        let Some(ui) = wk.upgrade() else {
                            return;
                        };
                        let st = ui.global::<AppState>();
                        st.set_scan_preview(make_preview_image(&data, w, h, fmt));
                    });
                });
                cam.set_frame_sink(call_dispatcher::on_camera_frame);
            }
            Err(err) => {
                state.set_scan_status(SharedString::from(format!("Failed to start camera: {err}")));
            }
        }
    });
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
            client.start_presence_loop();
            start_group_conn_sweeper();
            let approved = client
                .status()
                .ok()
                .map(|s| s == chatx_core::account::DeviceStatus::Approved)
                .unwrap_or(false);
            eprintln!("[login] device status approved={approved}");
            DEVICE_APPROVED.with(|c| c.set(approved));
            *CLIENT.lock().unwrap() = Some(client);
            set_current_me(&user_id);

            start_inbound_pump(weak.clone());

            // Start the moment (post) sync 60s background timer. The timer
            // is OnceLock-guarded so a re-login never double-spawns. It only
            // fires a sync round; if nothing has changed it returns quickly.
            moment::start_timer(weak.clone());

            if let Some(pool) = pool {
                let pool = Arc::new(pool);
                *POOL.lock().unwrap() = Some(pool.clone());
                // Restore the persisted UI language (stored in the `settings`
                // table) so it takes effect for the whole session, including the
                // auth screen. Reading the DB is async; Slint calls must run on
                // the event-loop thread, so hop onto it via invoke_from_event_loop.
                {
                    let lang_pool = pool.clone();
                    let lang_weak = weak.clone();
                    runtime().spawn(async move {
                        let Some(lang) = sqlite::settings::get(&lang_pool, "lang")
                            .await
                            .ok()
                            .flatten()
                        else {
                            return;
                        };
                        if lang.is_empty() {
                            return;
                        }
                        let lang2 = lang.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let _ = slint::select_bundled_translation(&lang2);
                            if let Some(ui) = lang_weak.upgrade() {
                                ui.global::<AppState>()
                                    .set_lang(SharedString::from(lang2.clone()));
                            }
                            slint::update_all_translations();
                        });
                    });
                }
                // Restore the persisted theme (light/dark). Applied on the
                // event-loop thread alongside the Slint global + Rust renderer.
                {
                    let theme_pool = pool.clone();
                    let theme_weak = weak.clone();
                    runtime().spawn(async move {
                        let Some(theme) = sqlite::settings::get(&theme_pool, "theme")
                            .await
                            .ok()
                            .flatten()
                        else {
                            return;
                        };
                        let dark = theme == "dark";
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = theme_weak.upgrade() {
                                ui.global::<Theme>().set_dark(dark);
                            }
                            if let Some(r) = RENDERER.get() {
                                r.lock().unwrap().set_theme(if dark {
                                    render::theme::Theme::dark()
                                } else {
                                    render::theme::Theme::default()
                                });
                            }
                        });
                    });
                }
                let rt = runtime();
                let weak = weak.clone();
                let me = user_id.clone();
                rt.spawn(async move {
                    match data::load(&pool, &me).await {
                        Ok((mut backend, nickname)) => {
                            // Load the friend-request inbox + sent for this device.
                            let me_b58 = peer.clone();
                            let _ = data::load_fr_requests(&pool, &me_b58, &mut backend).await;
                            let backend = Arc::new(tokio::sync::RwLock::new(backend));
                            let _ = slint::invoke_from_event_loop(move || {
                                *BACKEND.lock().unwrap() = Some(backend.clone());
                                if let Some(ui) = weak.upgrade() {
                                    let state = ui.global::<AppState>();
                                    state.set_peer_id(SharedString::from(peer.clone()));
                                    state.set_qr_image(qr_image(&peer));
                                    state.set_nickname(SharedString::from(nickname));
                                    state.set_discover_shown(DISCOVER_INITIAL);
                                    publish_to_views(&state, backend);
                                }
                            });
                        }
                        Err(err) => {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(ui) = weak.upgrade() {
                                    ui.global::<AppState>().set_data_status(SharedString::from(
                                        format!("load failed: {err}"),
                                    ));
                                }
                            });
                        }
                    }
                });
            }
        }
        Err(err) => {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>()
                    .set_auth_message(SharedString::from(err.to_string()));
                ui.set_logged_in(false);
            }
            return;
        }
    }

    if let Some(ui) = weak.upgrade() {
        ui.global::<AppState>()
            .set_auth_message(SharedString::from(format!("logged in:{user_id}")));
        ui.global::<AppState>()
            .set_user_id(SharedString::from(user_id));
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
        let mut server_poll = tokio::time::interval(std::time::Duration::from_secs(2));
        server_poll.tick().await;
        loop {
            let client: std::sync::Arc<chatx_core::Client<chatx_core::signal::HttpDirectory>> =
                match CLIENT.lock().unwrap().clone() {
                    Some(c) => c,
                    None => break,
                };
            tokio::select! {
                _ = server_poll.tick() => {
                    let _touched = client.poll_group_server_inbox().await;
                    continue;
                }
                maybe_evt = client.next_event() => {
                    let Some(evt) = maybe_evt else {
                        break;
                    };
                    if let chatx_core::swarm::ChatEvent::Webrtc { peer, json, .. } = &evt {
                        let client_arc = client.clone();
                        let peer = *peer;
                        let json = json.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            call::handle_signal_offer(client_arc, peer, &json);
                        });
                        continue;
                    }
                    if let chatx_core::swarm::ChatEvent::Text { peer, req } = &evt {
                        if let Some(text) = req.text.as_deref() {
                            if let Some(q) = FriendQ::decode(text) {
                                let peer_b58 = peer.to_base58();
                                handle_inbound_friendq(weak.clone(), &peer_b58, q).await;
                                continue;
                            }
                        }
                    }
                    let touched = client.process_event(evt).await;
                    if let Some(touched) = touched {
                        refresh_inbound_chat(weak.clone(), touched).await;
                    }
                }
            }
        }
    });
}

/// Handle an inbound friendq frame (someone sent us Inquire / Accept / Reject
/// over P2P). Persists the local copy, refreshes the UI, and sends any
/// required local contact update on acceptance.
async fn handle_inbound_friendq(weak: slint::Weak<MainWindow>, from_peer: &str, q: FriendQ) {
    eprintln!(
        "[friendq] INBOUND action={:?} from={from_peer} req={} rec={}",
        q.action, q.requester, q.recipient
    );
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool.clone(), backend.clone()) else {
        return;
    };

    // Only frames where *I* am the recipient of the decision act on me. Inbound
    // Accept / Reject from the peer means they processed *my* request.
    match q.action {
        FriendQAction::Inquire => {
            // Someone is asking to add me. Persist the pair and reflect it in
            // the inbox. The sender's self-reported name (username) is used
            // as the display hint; fall back to peer-id resolution if absent.
            let me_b58 = match CLIENT.lock().unwrap().clone() {
                Some(c) => c.peer_base58(),
                None => return,
            };
            if from_peer == me_b58 {
                return;
            }
            let note = if q.sender_name.trim().is_empty() {
                data::name_for_peer(&pool, from_peer).await
            } else {
                q.sender_name.clone()
            };
            let _ = sqlite::social::frq_upsert(&pool, from_peer, &me_b58, Some(&note)).await;
            {
                let mut g = backend.write().await;
                g.add_friend_request(from_peer, note, sqlite::now_ms(), data::FrqDir::Inbox);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let state = ui.global::<AppState>();
                    publish_to_views(&state, backend.clone());
                }
            });
        }
        FriendQAction::Accept => {
            // The peer accepted *my* request. Add them as a contact locally.
            let me = current_me();
            let me_b58 = match CLIENT.lock().unwrap().clone() {
                Some(c) => c.peer_base58(),
                None => return,
            };
            if from_peer == me_b58 {
                return;
            }
            let other = if q.recipient == me_b58 {
                q.requester
            } else {
                q.recipient
            };
            // In my local DB the row is (requester=me, recipient=other).
            let _ = sqlite::social::frq_set_status(
                &pool,
                &me_b58,
                &other,
                sqlite::social::FRQ_ACCEPTED,
            )
            .await;
            let name = if q.sender_name.trim().is_empty() {
                data::name_for_peer(&pool, &other).await
            } else {
                q.sender_name.clone()
            };
            if let Err(e) = do_add(&pool, &me, &other, &name, &backend).await {
                eprintln!("[friendq] inbound accept failed to add contact: {e}");
            }
            {
                let mut g = backend.write().await;
                g.remove_friend_request_if(&other, data::FrqDir::Sent);
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    let state = ui.global::<AppState>();
                    publish_to_views(&state, backend.clone());
                    state.set_notice_text(SharedString::from("Your add request was accepted"));
                }
            });
        }
        FriendQAction::Reject => {
            if let Some(me_b58) = { CLIENT.lock().unwrap().clone().map(|c| c.peer_base58()) } {
                if from_peer == me_b58 {
                    return;
                }
                let other = if q.recipient == me_b58 {
                    q.requester
                } else {
                    q.recipient
                };
                // Same role as in the sender's local row.
                let _ = sqlite::social::frq_set_status(
                    &pool,
                    &me_b58,
                    &other,
                    sqlite::social::FRQ_REJECTED,
                )
                .await;
                {
                    let mut g = backend.write().await;
                    g.remove_friend_request_if(&other, data::FrqDir::Sent);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        let state = ui.global::<AppState>();
                        publish_to_views(&state, backend.clone());
                        state.set_notice_text(SharedString::from(
                            "The other party declined your add request",
                        ));
                    }
                });
            }
        }
    }
}

async fn refresh_inbound_chat(weak: slint::Weak<MainWindow>, chat_id: String) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else {
        return;
    };
    let me = current_me();
    if me.is_empty() {
        eprintln!("[inbound] abort: no client/me yet");
        return;
    }
    eprintln!("[inbound] refresh chat_id={chat_id} me={me}");
    {
        let mut b: data::DataBackend = backend.read().await.clone();
        let _ = data::refresh_chat_row(&pool, &me, &mut b, &chat_id).await;
        *backend.write().await = b;
    }
    touch_chat_activity(&chat_id);
    let is_current = CURRENT_CHAT.read().unwrap().as_deref() == Some(chat_id.as_str());
    let key = {
        backend
            .read()
            .await
            .chats
            .iter()
            .find(|r| r.chat_id == chat_id)
            .map(|r| r.key)
    };
    eprintln!("[inbound] chat_id={chat_id} key={key:?} is_current={is_current}");
    let weak2 = weak.clone();
    let backend2 = backend.clone();
    let k = key;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak2.upgrade() {
            let state = ui.global::<AppState>();
            publish_to_views(&state, backend2.clone());
            if is_current {
                if let Some(k) = k {
                    load_chat_messages(ui.as_weak(), k);
                }
            }
        }
    });
}

fn publish_to_views(state: &AppState, backend: ArcBackend) {
    let mut snapshot = backend.blocking_read().clone();

    let rank = |c: char| if c == '#' { '[' } else { c };
    snapshot.contacts.sort_by(|a, b| {
        rank(data::contact_letter(&a.name))
            .cmp(&rank(data::contact_letter(&b.name)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

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
        if letter_entries
            .last()
            .map(|(c, _)| *c)
            .map_or(true, |c| c != ltr)
        {
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

    // Friend requests: only the inbox (requests to add *me*) is shown in the
    // Contacts list; my sent requests are surfaced separately in the
    // add-contact flow.
    let friend_requests: Vec<FriendRequestRow> = snapshot
        .friend_requests
        .iter()
        .filter(|r| r.dir == data::FrqDir::Inbox)
        .map(|r| FriendRequestRow {
            key: r.id,
            peer_id: SharedString::from(r.peer_id.clone()),
            name: SharedString::from(r.name.clone()),
            time_label: SharedString::from(fmt_time(r.time_ms)),
        })
        .collect();
    state.set_friend_requests(slint::ModelRc::new(slint::VecModel::from(friend_requests)));

    publish_discover(state, &backend);

    state.set_data_status(SharedString::new());
}

/// Bottom-lazy loading: decode and surface only the first `DISCOVER_INITIAL`
/// covers on demand, then pull in the next `DISCOVER_STEP` rows when the user
/// scrolls near the bottom until every row is shown. Off-screen posts never get
/// their cover bitmap decoded, keeping memory bounded.
const DISCOVER_INITIAL: i32 = 8;
const DISCOVER_STEP: i32 = 2;

/// Publish the slice we have reached so far (rows 0..discover-shown). All
/// rows stay in the backend; the not-yet-shown ones just lack a decoded cover.
fn publish_discover(state: &AppState, backend: &ArcBackend) {
    let snapshot = backend.blocking_read();
    let total = snapshot.discover.len() as i32;
    // `discover-shown` is a monotonically-growing reveal cursor (how many rows
    // the user has scrolled to see). We must NOT clamp it down to `total` —
    // doing so shrank it (e.g. 8 -> 1 for a one-row feed), and then publishing
    // a *new* post left the cursor at 1 so only the freshly-pushed front row
    // rendered and the previously-seen posts disappeared until an app restart
    // re-initialised the cursor. The display slice is bounded by `total`; the
    // stored cursor only ever advances.
    let cursor = state.get_discover_shown().max(0);
    let visible = cursor.min(total);
    let discover: Vec<DiscoverCard> = snapshot
        .discover
        .iter()
        .take(visible as usize)
        .map(|r| DiscoverCard {
            id: r.key,
            title: SharedString::from(r.title.clone()),
            user: SharedString::from(r.author.clone()),
            likes: r.likes as i32,
            liked: r.liked,
            image: load_image_cached(&r.cover),
        })
        .collect();
    state.set_discover(slint::ModelRc::new(slint::VecModel::from(discover)));
    state.set_discover_shown(cursor);
    state.set_discover_has_more(total > cursor);
}

/// Fetch the next discover batch when the user scrolls to the trigger area.
fn discover_load_more(weak: slint::Weak<MainWindow>) {
    let backend = BACKEND.lock().unwrap().clone();
    let Some(backend) = backend else {
        return;
    };
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let st = ui.global::<AppState>();
    if !st.get_discover_has_more() {
        return;
    }
    st.set_discover_shown(st.get_discover_shown() + DISCOVER_STEP);
    publish_discover(&st, &backend);
}

fn publish_chat_rows(state: &AppState, backend: ArcBackend) {
    let t0 = std::time::Instant::now();
    eprintln!("[chat] publish_chat_rows: enter");
    let snapshot = backend.blocking_read().clone();
    eprintln!(
        "[chat] publish_chat_rows: +{}ms snapshot chats={}",
        t0.elapsed().as_millis(),
        snapshot.chats.len()
    );

    {
        let pool = POOL.lock().unwrap().clone();
        let me_peer = CLIENT.lock().unwrap().as_ref().map(|c| c.peer_base58());
        let mut friend_by_peer: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        for c in &snapshot.contacts {
            if !c.peer_id.is_empty() {
                friend_by_peer.insert(c.peer_id.clone(), c.user_id);
            }
        }
        let dms: Vec<(String, String, i64)> = snapshot
            .chats
            .iter()
            .filter(|c| !c.is_group)
            .map(|c| (c.chat_id.clone(), c.peer_id.clone(), c.time_ms))
            .collect();
        if let (Some(pool), Some(me_peer)) = (pool, me_peer) {
            if !dms.is_empty() {
                let rt = runtime();
                rt.spawn(async move {
                    for (chat_id, row_peer, time_ms) in dms {
                        let pb = if !row_peer.is_empty() && row_peer != me_peer {
                            Some(row_peer)
                        } else if chat_id.contains('|') {
                            chat_id
                                .split('|')
                                .find(|p| !p.is_empty() && *p != me_peer)
                                .map(|s| s.to_string())
                        } else {
                            None
                        };
                        let Some(pb) = pb else {
                            continue;
                        };
                        let conv_id = match sqlite::conversations::ensure_dm(&pool, &chat_id).await
                        {
                            Ok(id) => id,
                            Err(e) => {
                                eprintln!("[chat] ensure_dm({chat_id}) failed: {e}");
                                continue;
                            }
                        };
                        match friend_by_peer.get(&pb).copied() {
                            Some(fuid) => {
                                let _ = sqlite::devices::bind_device(&pool, fuid, &pb).await;
                            }
                            None => {}
                        }
                        let _ = sqlite::conversations::set_peer_id(&pool, conv_id, &pb).await;
                        if time_ms > 0 {
                            let _ =
                                sqlite::conversations::ensure_time(&pool, conv_id, time_ms).await;
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
    eprintln!(
        "[chat] publish_chat_rows: +{}ms done (chats={})",
        t0.elapsed().as_millis(),
        snapshot.chats.len()
    );
}

fn load_chat_messages(ui_weak: slint::Weak<MainWindow>, chat_key: i32) {
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
        eprintln!("[chat] load_chat_messages{chat_key}: chat_id_for returned None");
        return;
    };
    set_current_chat_id(&chat_id);
    group_conn_enter(&chat_id);
    if !chat_id.starts_with("grp-") {
        let me_peer = client.peer_base58();
        let peer: Option<String> = {
            let g = backend.blocking_read();
            let from_row = g
                .chats
                .iter()
                .find(|r| r.key == chat_key)
                .map(|r| r.peer_id.clone())
                .filter(|p| !p.is_empty() && p != &me_peer);
            from_row.or_else(|| {
                if chat_id.contains('|') {
                    chat_id
                        .split('|')
                        .find(|p| !p.is_empty() && *p != me_peer)
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        };
        match peer {
            Some(p) => {
                let client = client.clone();
                std::thread::spawn(move || {
                    let _ = client.warm_dial_peer(&p);
                });
            }
            None => eprintln!(
                "[chat] load_chat_messages{chat_key}: no peer id resolved (chat_id={chat_id})"
            ),
        }
    } else {
        eprintln!(
            "[chat] load_chat_messages{chat_key}: group chat, skipping dm warm_dial (chat_id={chat_id})"
        );
    }
    let me_peer = client.peer_base58().to_string();
    let me = client.user_id().to_string();
    let is_group_chat = chat_id.starts_with("grp-");
    let title = {
        let g = backend.blocking_read();
        g.chats
            .iter()
            .find(|r| r.key == chat_key)
            .map(|r| r.title.clone())
            .unwrap_or_else(|| {
                if is_group_chat {
                    "Group".into()
                } else {
                    chat_id.clone()
                }
            })
    };
    let rt = runtime();
    let ui_weak2 = ui_weak.clone();

    let (available_w, scale) = if let Some(ui) = ui_weak2.upgrade() {
        let state = ui.global::<AppState>();
        let window_w = state.get_xp().max(320.0);
        let pane_w = if state.get_is_mobile() {
            window_w
        } else {
            (window_w - 338.0).max(320.0)
        };
        let dpr = ui.window().scale_factor().max(1.0);
        (pane_w as u32, dpr)
    } else {
        (390, 2.0)
    };
    rt.spawn(async move {
        let rows = match client.store().load(&chat_id, 200, 0).await {
            Ok(rows) => rows,
            Err(e) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.global::<ChatSession>()
                            .set_send_status(SharedString::from(format!(
                                "Failed to load messages: {e}"
                            )));
                    }
                });
                return;
            }
        };

        ensure_renderer();

        let mut rows = rows;
        rows.sort_by(|a, b| b.t.cmp(&a.t).then(b.id.cmp(&a.id)));
        let rows: Vec<chatx_core::store::StoredMsg> = rows
            .into_iter()
            .map(|m| {
                if !is_group_chat {
                    return Some(m);
                }
                match client.open_stored_group_message(&chat_id, &m.text) {
                    Ok(plain) => Some(chatx_core::store::StoredMsg { text: plain, ..m }),
                    Err(e) => {
                        eprintln!("[chat] failed to open group msg in {chat_id}: {e}; hiding it");
                        None
                    }
                }
            })
            .flatten()
            .collect();
        let mut sender_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if is_group_chat {
            for sender in rows
                .iter()
                .map(|m| m.sender.clone())
                .collect::<std::collections::HashSet<_>>()
            {
                sender_names.insert(
                    sender.clone(),
                    crate::data::group_sender_name(&pool, &sender).await,
                );
            }
        }
        let rendered: Vec<RenderedMsg> = {
            let mut r = RENDERER.get().unwrap().lock().unwrap();
            rows.iter()
                .map(|m| {
                    let is_self = m.sender == me_peer || m.sender == me;
                    let time = time_label(m.t as i64);
                    let sender = if is_self {
                        String::new()
                    } else if is_group_chat {
                        sender_names
                            .get(&m.sender)
                            .cloned()
                            .unwrap_or_else(|| m.sender.clone())
                    } else {
                        title.clone()
                    };
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    std::hash::Hash::hash(&m.text, &mut h);
                    std::hash::Hash::hash(&sender, &mut h);
                    std::hash::Hash::hash(&is_self, &mut h);
                    std::hash::Hash::hash(&time, &mut h);

                    let avatar = Some(make_avatar(&m.sender, 80));
                    let msg = ChatMsg {
                        sender,
                        side: if is_self { Side::SelfSide } else { Side::Other },
                        time,
                        text: m.text.clone(),
                        avatar,
                    };
                    render_msg(
                        &mut r,
                        std::hash::Hasher::finish(&h),
                        &msg,
                        available_w,
                        scale,
                    )
                })
                .collect()
        };
        let dist: f32 = {
            CHAT_SCROLL_POS
                .read()
                .unwrap()
                .get(&chat_key)
                .copied()
                .unwrap_or(0.0)
        };
        let _ = slint::invoke_from_event_loop(move || {
            let ui = match ui_weak2.upgrade() {
                Some(ui) => ui,
                None => return,
            };
            let cs = ui.global::<ChatSession>();

            const PAD: f32 = 12.0;
            const GAP: f32 = 12.0;
            let n = rendered.len();
            let heights: Vec<f32> = rendered.iter().map(|m| m.height as f32 / scale).collect();
            let sum_all: f32 = heights.iter().sum();
            let messages: Vec<MessageData> = rendered
                .into_iter()
                .enumerate()
                .map(|(i, m)| {
                    let older_sum: f32 = heights.iter().skip(i + 1).sum();
                    let rows_above = (n.saturating_sub(1).saturating_sub(i)) as f32;
                    let top = PAD + rows_above * GAP + older_sum;
                    to_message_data(m, scale, top)
                })
                .collect();
            let content_h = if n == 0 {
                0.0
            } else {
                (n - 1) as f32 * GAP + sum_all + 2.0 * PAD
            };
            cs.set_scroll_dist(dist);
            cs.set_scroll_token(cs.get_scroll_token() + 1);
            cs.set_content_h(content_h);
            cs.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
            cs.set_send_status(SharedString::new());
        });
    });
}

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
    let Some(chat_id) = chat_id else {
        eprintln!("[send] no chat_id for key={key}, aborting");
        return;
    };
    let is_group = chat_id.starts_with("grp-");
    eprintln!(
        "[send] key={key} chat_id={chat_id} is_group={is_group} body_len={}",
        body.len()
    );

    if is_group {
        if let Some(ui) = weak.upgrade() {
            ui.global::<ChatSession>()
                .set_send_status(SharedString::from("Sending…"));
        }
        let rt = runtime();
        let weak2 = weak.clone();
        rt.spawn(async move {
            eprintln!("[send] group message -> chat_id={chat_id}");
            match client.send_group_message(&chat_id, &body).await {
                Ok(_) => {
                    eprintln!("[send] group message ok");
                    touch_chat_activity(&chat_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<ChatSession>()
                                .set_send_status(SharedString::new());
                        }
                    });
                    CHAT_SCROLL_POS.write().unwrap().insert(key, 0.0);
                    load_chat_messages(weak2, key);
                }
                Err(e) => {
                    eprintln!("[send] group message FAILED: {e}");
                    let msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<ChatSession>()
                                .set_send_status(SharedString::from(format!("Send failed: {msg}")));
                        }
                    });
                }
            }
        });
        return;
    }

    if let Some(ui) = weak.upgrade() {
        ui.global::<ChatSession>()
            .set_send_status(SharedString::from("Sending…"));
    }
    let row_peer: String = {
        backend
            .blocking_read()
            .chats
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.peer_id.clone())
            .unwrap_or_default()
    };
    let me = client.peer_base58();
    let me_peers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        if !row_peer.is_empty() && row_peer != me {
            v.push(row_peer.clone());
        }
        if chat_id.contains('|') {
            for part in chat_id.split('|') {
                if !part.is_empty() && part != me && !v.iter().any(|x| x == part) {
                    v.push(part.to_string());
                }
            }
        }
        if v.is_empty() {
            eprintln!("[send] no known peer for chat {chat_id}; cannot send");
        }
        v
    };
    let rt = runtime();
    let weak2 = weak.clone();
    let my_peer = client.peer_base58();
    eprintln!("[send] dm candidates: {me_peers:?}, me={my_peer}");
    rt.spawn(async move {
        let mut last_err: String = if me_peers.is_empty() {
            "Cannot determine peer (conversation missing peer id)".to_string()
        } else {
            String::new()
        };
        let mut sent = false;
        'outer: for cand in &me_peers {
            if cand == &my_peer {
                eprintln!("[send] skip self {cand}");
                continue;
            }
            eprintln!("[send] dm trying peer={cand}");
            match client.send_dm(cand, &body).await {
                Ok(_) => {
                    eprintln!("[send] dm ok -> {cand}");
                    touch_chat_activity(&chat_id);
                    sent = true;
                    break 'outer;
                }
                Err(e) => {
                    eprintln!("[send] dm peer={cand} failed: {e}");
                    last_err = e.to_string();
                }
            }
        }
        if !sent {
            let msg = if last_err.trim().is_empty() {
                "Peer is offline".to_string()
            } else {
                last_err
            };
            eprintln!("[send] dm ALL candidates failed: {msg}");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ChatSession>()
                        .set_send_status(SharedString::from(format!("Send failed: {msg}")));
                }
            });
            return;
        }
        {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ChatSession>()
                        .set_send_status(SharedString::new());
                }
            });
            load_chat_messages(weak2, key);
        }
    });
}

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
    let me = current_me();
    if let Some(ui) = weak.upgrade() {
        publish_to_views(&ui.global::<AppState>(), backend.clone());
    }
    if let Some(post_id) = post_id {
        let rt = runtime();
        rt.spawn(async move {
            let _ = data::persist_like_toggle(&pool, &me, post_id, want_liked).await;
        });
    }
}

fn layout_my_notes(notes: &[data::NoteRowData]) -> Vec<NoteRow> {
    // Column geometry / per-card heights are handled entirely in Slint: a
    // single-column `ScrollView` + `VerticalLayout` sizes each card to its
    // content's own preferred height, so the body never gets clipped and the
    // scroll range always reaches the last card's action bar. We only project
    // the stored notes into UI rows (preserving newest-first order).
    notes
        .iter()
        .map(|n| NoteRow {
            id: n.key,
            content: SharedString::from(n.content.clone()),
            created_label: SharedString::from(fmt_time(n.created_at)),
        })
        .collect()
}

fn publish_my_notes(state: &AppState, backend: &ArcBackend) {
    let notes = backend.blocking_read().notes.clone();
    let rows = layout_my_notes(&notes);
    state.set_my_notes(slint::ModelRc::new(slint::VecModel::from(rows)));
}

fn refresh_my_notes(weak: slint::Weak<MainWindow>) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let Some(backend) = backend else {
        return;
    };
    let me = current_me();
    if me.is_empty() {
        return;
    }
    let Some(pool) = pool else {
        return;
    };
    let rt = runtime();
    rt.spawn(async move {
        let me_id = sqlite::users::get_by_username(&pool, &me)
            .await
            .ok()
            .flatten()
            .map(|u| u.id)
            .unwrap_or(0);
        let stored = sqlite::notes::list_notes(&pool, me_id)
            .await
            .unwrap_or_default();
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

fn save_my_note(weak: slint::Weak<MainWindow>, content: String) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else {
        return;
    };
    let me = current_me();
    if me.is_empty() || content.trim().is_empty() {
        return;
    }
    let rt = runtime();
    rt.spawn(async move {
        let me_id = match sqlite::users::get_by_username(&pool, &me).await {
            Ok(Some(u)) => u.id,
            Ok(None) | Err(_) => {
                show_app_error(weak.clone(), String::from("Failed to save note"));
                return;
            }
        };
        let created = match sqlite::notes::create_note(&pool, me_id, &content).await {
            Ok(n) => n,
            Err(_) => {
                show_app_error(weak.clone(), String::from("Failed to save note"));
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
            show_app_notice(weak, String::from("Note saved"), true);
        });
    });
}

fn delete_my_note(weak: slint::Weak<MainWindow>, key: i32) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else {
        return;
    };
    let note_id = { backend.blocking_read().note_id_for(key) };
    let Some(note_id) = note_id else {
        return;
    };
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

fn copy_note_to_clipboard(text: String) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text);
    }
}
// ─────────────── Share / publish moment helpers ──────────────────────────
/// Open + start the camera and pump preview frames into `CameraState.preview`.
/// Idempotent: if the camera is already running, the sinks are simply
/// re-installed. Runs on the UI thread (same as the QR scanner); Apple's
/// TCC prompt is a system modal, so a brief UI pause matches platform
/// convention.
fn start_camera_capturer(weak: slint::Weak<MainWindow>) {
    if let Some(ui) = weak.upgrade() {
        let st = ui.global::<CameraState>();
        st.set_capturing(false);
        st.set_last_captured(SharedString::new());
        st.set_preview(slint::Image::default());
        st.set_status(SharedString::from("Starting camera…"));
    }

    CAMERA.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(Camera::new());
        }
        let mut guard = slot.borrow_mut();
        let cam = guard.as_mut().expect("cap camera inited");

        // Install the photo-capture sink (result delivered async via the
        // platform shim after `capture()`).
        {
            let weak = weak.clone();
            cam.set_capture_sink(move |res: camera::CaptureResult| {
                let wk = weak.clone();
                let wk2 = weak.clone();
                match res {
                    Ok(path) => {
                        eprintln!("[cap] capture saved → {path}");
                        append_moment_media(wk.clone(), path);
                        on_cap_done(wk2);
                    }
                    Err(err) => {
                        eprintln!("[cap] capture failed: {err}");
                        set_cap_status(wk.clone(), format!("Failed to take photo: {err}"));
                        on_cap_done(wk2);
                    }
                }
            });
        }

        // Install the preview-frame sink (fires on every frame; build the
        // Slint image inside the event-loop hop since `SlintImage` is not
        // Send).
        {
            let weak3 = weak.clone();
            cam.set_frame_sink(move |bytes: &[u8], w: u32, h: u32, fmt: u32| {
                if w == 0 || h == 0 {
                    return;
                }
                let data = bytes.to_vec();
                let wk = weak3.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = wk.upgrade() else {
                        return;
                    };
                    let st = ui.global::<CameraState>();
                    st.set_preview(make_preview_image(&data, w, h, fmt));
                    // First real frame means "camera is up" — clear the
                    // startup status only if it's still the initial value.
                    if st.get_status() == "Starting camera…" {
                        st.set_status(SharedString::new());
                    }
                });
            });
        }

        match cam.start() {
            Ok(()) => {
                eprintln!("[cap] camera running");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<CameraState>().set_status(SharedString::new());
                    }
                });
            }
            Err(err) => {
                eprintln!("[cap] start failed: {err}");
                set_cap_status(weak, format!("Failed to start camera: {err}"));
            }
        }
        let _ = guard;
    });
}

fn set_cap_status(weak: slint::Weak<MainWindow>, msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<CameraState>()
                .set_status(SharedString::from(msg));
        }
    });
}

fn on_cap_done(weak: slint::Weak<MainWindow>) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<CameraState>().set_capturing(false);
        }
    });
}

/// Trigger a still capture (the camera session is expected to already be
/// running). Re-entry is gated in the UI by `CameraState.capturing`
/// (the shutter TouchArea checks `!capturing`), which is set true here and
/// cleared again by [`on_cap_done`] when the async result lands.
fn trigger_photo_capture(weak: slint::Weak<MainWindow>) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<CameraState>().set_capturing(true);
    }
    let res: Option<Result<(), String>> = CAMERA.with(|slot| match slot.borrow_mut().as_mut() {
        Some(c) => Some(c.capture()),
        None => None,
    });
    match res {
        Some(Ok(())) => {
            // Result arrives asynchronously: Apple via the Swift shim, desktop
            // on the next preview frame. Both land in the capture sink, which
            // calls `on_cap_done` to clear `capturing`.
        }
        Some(Err(e)) => {
            on_cap_done(weak.clone());
            set_cap_status(weak, format!("Failed to take photo: {e}"));
        }
        None => {
            on_cap_done(weak.clone());
            set_cap_status(weak, "Camera not started".to_string());
        }
    }
}

fn stop_camera_capturer(weak: slint::Weak<MainWindow>) {
    // Stop the camera session + close overlay + clear the draft.
    CAMERA.with(|slot| {
        if let Some(cam) = slot.borrow_mut().as_mut() {
            cam.stop();
        }
    });
    reset_moment_draft();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let st = ui.global::<AppState>();
            let mut nav = st.get_nav_state();
            nav.global_overlay = GlobalOverlayType::None;
            st.set_nav_state(nav);
            let ms = ui.global::<MomentState>();
            ms.set_title(SharedString::new());
            ms.set_content(SharedString::new());
            ms.set_status(SharedString::new());
            ms.set_publishing(false);
            ms.set_media(slint::ModelRc::new(slint::VecModel::from(
                Vec::<SlintImage>::new(),
            )));
        }
    });
}

/// Top-right "Finish": close the camera overlay + push the publish page.
fn finish_camera_and_open_publish(weak: slint::Weak<MainWindow>) {
    // We stop the camera session (preview stops), but keep MOMENT_MEDIA
    // populated so the publish page sees the draft.
    CAMERA.with(|slot| {
        if let Some(cam) = slot.borrow_mut().as_mut() {
            cam.stop();
        }
    });
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            let st = ui.global::<AppState>();
            let mut nav = st.get_nav_state();
            nav.global_overlay = GlobalOverlayType::None;
            st.set_nav_state(nav);
            push_sub_history(
                &st,
                SubPageEntry {
                    page: SubPageType::AddMoment,
                    payload: 0,
                },
            );
        }
    });
}

/// Append a saved photo path to the draft + refresh the UI thumbnails.
fn append_moment_media(weak: slint::Weak<MainWindow>, path: String) {
    {
        let mut m = MOMENT_MEDIA.lock().unwrap();
        if m.len() >= 9 {
            // Cap; drop the newest (we keep the first 9).
            eprintln!("[moment] draft already has 9 photos; ignoring {path}");
        } else {
            m.push(path.clone());
        }
    }
    refresh_moment_media_ui(weak);
}

fn remove_moment_media(weak: slint::Weak<MainWindow>, i: i32) {
    {
        let mut m = MOMENT_MEDIA.lock().unwrap();
        if i < 0 || (i as usize) >= m.len() {
            return;
        }
        let removed = m.remove(i as usize);
        let _ = std::fs::remove_file(std::path::Path::new(&removed));
    }
    refresh_moment_media_ui(weak);
}

/// Clear the draft. Any source files still on disk (temp-dir camera/album
/// copies, or pre-publish originals) are removed — the persistent copies live
/// in the post store and are tracked separately in `MOMENT_MEDIA`'s callers.
fn reset_moment_draft() {
    let paths: Vec<String> = {
        let mut m = MOMENT_MEDIA.lock().unwrap();
        std::mem::take(&mut *m)
    };
    for p in &paths {
        let _ = std::fs::remove_file(std::path::Path::new(p));
    }
}

/// Republish `MOMENT_MEDIA` as a `[image]` model on `MomentState.media`.
/// The Slint `Image` objects are not `Send`, so each is loaded inside the
/// event-loop hop.
fn refresh_moment_media_ui(weak: slint::Weak<MainWindow>) {
    let paths = MOMENT_MEDIA.lock().unwrap().clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let imgs: Vec<SlintImage> = paths
            .iter()
            .filter_map(|p| SlintImage::load_from_path(std::path::Path::new(p)).ok())
            .collect();
        ui.global::<MomentState>()
            .set_media(slint::ModelRc::new(slint::VecModel::from(imgs)));
    });
}

/// Called by the `album` backend (from whatever thread the picker
/// finished on) with the list of photo paths the user selected. Empty ⇒
/// cancel / nothing picked.
fn on_moment_photos_selected(paths: Vec<String>) {
    if paths.is_empty() {
        eprintln!("[moment] empty selection (cancelled)");
        return;
    }
    {
        let mut m = MOMENT_MEDIA.lock().unwrap();
        for p in paths {
            if m.len() >= 9 {
                break;
            }
            m.push(p);
        }
    }
    let weak = match call::weak_window() {
        Some(w) => w,
        None => return,
    };
    refresh_moment_media_ui(weak);
}

/// Called by MomentState on_publish. Validates the draft, persists the post
/// to sqlite (creating a `social_posts` row), then pops the subpage.
fn publish_moment(weak: slint::Weak<MainWindow>) {
    let pool = POOL.lock().unwrap().clone();
    let backend = BACKEND.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else {
        set_moment_status(
            weak.clone(),
            "Not signed in or backend not ready".to_string(),
        );
        return;
    };
    let me = current_me();
    if me.is_empty() {
        set_moment_status(weak.clone(), "Not signed in".to_string());
        return;
    }
    // Read the draft (copy out before async work).
    let title = {
        if let Some(ui) = weak.upgrade() {
            ui.global::<MomentState>().get_title().to_string()
        } else {
            String::new()
        }
    };
    let content = {
        if let Some(ui) = weak.upgrade() {
            ui.global::<MomentState>().get_content().to_string()
        } else {
            String::new()
        }
    };
    let media_paths: Vec<String> = MOMENT_MEDIA.lock().unwrap().clone();
    if title.trim().is_empty() && content.trim().is_empty() && media_paths.is_empty() {
        set_moment_status(
            weak.clone(),
            "Add at least a title, body, or a photo".to_string(),
        );
        return;
    }
    set_moment_status(weak.clone(), "Publishing…".to_string());
    if let Some(ui) = weak.upgrade() {
        ui.global::<MomentState>().set_publishing(true);
    }

    let rt = runtime();
    let weak2 = weak.clone();
    rt.spawn(async move {
        // Resolve me_id
        let me_id = match sqlite::users::get_by_username(&pool, &me).await {
            Ok(Some(u)) => u.id,
            _ => {
                set_moment_status(weak2.clone(), "Failed to look up user".to_string());
                if let Some(ui) = weak2.upgrade() {
                    ui.global::<MomentState>().set_publishing(false);
                }
                return;
            }
        };
        // Compose the body: title + content.
        let mut body = String::new();
        if !title.trim().is_empty() {
            body.push_str(title.trim());
            if !content.trim().is_empty() {
                body.push('\n');
            }
        }
        if !content.trim().is_empty() {
            body.push_str(content.trim());
        }
        // Move the chosen media into the persistent post store under
        // CHATX_HOME before persisting the post so the rows never point at
        // temp files that the OS may clean.
        let store_group = format!("p_{}", (sqlite::new_id() & 0x7fff_ffff_ffff) as u64);
        let mut stored_media: Vec<String> = Vec::with_capacity(media_paths.len());
        let mut stored_previews: Vec<String> = Vec::with_capacity(media_paths.len());
        let mut any_failed = false;
        for (i, src) in media_paths.iter().enumerate() {
            match camera::posts::store_original(std::path::Path::new(src), &store_group, (i + 1) as u32) {
                Ok(p) => {
                    let stored = p.to_string_lossy().into_owned();
                    // Fall back to the original as its own preview when the
                    // format is not decodable (e.g. HEIC), so the index in
                    // `stored_previews` matches `stored_media`.
                    let preview = match camera::posts::make_preview(std::path::Path::new(src), &store_group, (i + 1) as u32) {
                        Ok(pv) => pv.to_string_lossy().into_owned(),
                        Err(_) => stored.clone(),
                    };
                    stored_media.push(stored);
                    stored_previews.push(preview);
                }
                Err(e) => {
                    eprintln!("[moment] store_original failed for {src}: {e}");
                    any_failed = true;
                }
            }
        }
        if any_failed && stored_media.is_empty() {
            set_moment_status(weak2.clone(), "Failed to save photos".to_string());
            if let Some(ui) = weak2.upgrade() {
                ui.global::<MomentState>().set_publishing(false);
            }
            return;
        }
        let media_json = serde_json::to_string(&stored_media).unwrap_or_default();
        let preview_json = if !stored_previews.is_empty() {
            Some(serde_json::to_string(&stored_previews).unwrap_or_default())
        } else {
            None
        };
        let created = match sqlite::social_feed::create_post(
            &pool,
            me_id,
            Some(body.as_str()),
            Some(media_json.as_str()),
            preview_json.as_deref(),
            0,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[moment] create_post failed: {e}");
                set_moment_status(weak2.clone(), "Publish failed".to_string());
                if let Some(ui) = weak2.upgrade() {
                    ui.global::<MomentState>().set_publishing(false);
                }
                return;
            }
        };
        // Fire-and-forget notification to the signal server so friends can
        // learn about this new post (and sync it from us over the binary
        // moment channel). Failure here does NOT roll back the local publish.
        let client = crate::CLIENT.lock().unwrap().clone();
        if let Some(client) = client {
            let my_peer = client.peer_base58().to_string();
            let my_user = client.user_id().to_string();
            let created_ts = created.timestamp;
            let _ = client
                .dir()
                .notify_post(&my_user, &my_peer, created_ts);
            eprintln!(
                "[moment] notify_post -> server: user={my_user} peer={my_peer} ts={created_ts}"
            );
        }
        let _ = slint::invoke_from_event_loop(move || {
            // Insert the new row at the front of discover.
            let my_nickname = if let Some(ui) = weak2.upgrade() {
                ui.global::<AppState>().get_nickname().to_string()
            } else {
                String::new()
            };
            {
                let mut g = backend.blocking_write();
                let key = g.key_for_post(created.id);
                // Cover = preview (small) for the card; the detail view loads
                // the full-resolution original from `media_urls` separately.
                let cover = stored_previews
                    .first()
                    .cloned()
                    .or_else(|| stored_media.first().cloned())
                    .unwrap_or_default();
                let row = data::DiscoverRow {
                    key,
                    post_id: created.id,
                    title: body,
                    author: my_nickname,
                    likes: 0,
                    liked: false,
                    cover,
                };
                g.discover.insert(0, row);
            }
            if let Some(ui) = weak2.upgrade() {
                publish_to_views(&ui.global::<AppState>(), backend.clone());
                // Reset the draft state — otherwise `publishing` stays true and
                // the next time the user opens the publish page it's stuck in
                // "Publishing..." with a disabled button.
                let ms = ui.global::<MomentState>();
                ms.set_title(SharedString::new());
                ms.set_content(SharedString::new());
                ms.set_status(SharedString::new());
                ms.set_publishing(false);
                let empty: Vec<SlintImage> = Vec::new();
                ms.set_media(slint::ModelRc::new(slint::VecModel::from(empty)));
                // Pop the publish subpage.
                let st = ui.global::<AppState>();
                pop_sub_history(&st);
            }
            // Reset the draft: clears MOMENT_MEDIA and removes the temp-dir
            // sources (persistent copies already live in the post store).
            reset_moment_draft();
        });
    });
}

fn set_moment_status(weak: slint::Weak<MainWindow>, msg: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<MomentState>()
                .set_status(SharedString::from(msg));
        }
    });
}

fn moment_remove_photo(i: i32) {
    // `MOMENT_MEDIA` is the source of truth; the UI refresh needs a window.
    // Reuse the static `MainWindow` weak handle that `call.rs` already
    // installs at app start (so we don't have to thread `weak` through
    // the TouchArea callback).
    let weak = match call::weak_window() {
        Some(w) => w,
        None => return,
    };
    remove_moment_media(weak, i);
}

fn delete_contact(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let Some(backend) = backend else {
        return;
    };
    {
        let mut g = backend.blocking_write();
        g.remove_contact(key);
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            publish_to_views(&ui.global::<AppState>(), backend);
        }
    });
}

fn delete_chat(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(pool), Some(backend)) = (pool, backend) else {
        return;
    };
    // Capture the string chat id before the in-memory row is dropped — it is the
    // key that ties the UI row to its sqlite `conversations` record.
    let chat_id = {
        let g = backend.blocking_read();
        g.chat_id_for(key).map(|s| s.to_string())
    };
    {
        let mut g = backend.blocking_write();
        g.remove_chat(key);
    }
    if let Some(chat_id) = chat_id {
        let rt = runtime();
        rt.spawn(async move {
            match sqlite::conversations::resolve_id(&pool, &chat_id).await {
                Ok(Some(conv_id)) => {
                    // Messages reference the conversation (FK, no CASCADE), so
                    // clear them before removing the parent row.
                    let _ = sqlx::query("DELETE FROM messages WHERE conversation_id = ?1")
                        .bind(conv_id)
                        .execute(&*pool)
                        .await;
                    let _ = sqlite::conversations::delete(&pool, conv_id).await;
                }
                Ok(None) => {}
                Err(e) => eprintln!("[delete_chat] resolve conversation failed: {e}"),
            }
            // Groups live in `groups`/`group_members` too; the chat id is
            // `grp-<id>`, so clean up that row as well (idempotent for DMs).
            if let Some(num) = chat_id.strip_prefix("grp-") {
                if let Ok(id) = num.parse::<i64>() {
                    if let Err(e) = sqlite::groups::delete(&pool, id).await {
                        eprintln!("[delete_chat] delete group failed: {e}");
                    }
                }
            }
        });
    }
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            publish_to_views(&ui.global::<AppState>(), backend);
        }
    });
}

fn should_use_server_fanout(member_count: usize) -> bool {
    return member_count >= chatx_core::GROUP_P2P_MAX;
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
    let mut members: Vec<MemberInfo> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let usernames: Vec<String> = {
        let g = backend.blocking_read();
        let mut v: Vec<String> = Vec::new();
        for key in &picked {
            match g
                .username_for(*key)
                .map(|s| s.to_string())
                .or_else(|| g.peer_id_for(*key).map(|s| s.to_string()))
            {
                Some(u) => v.push(u),
                None => failed.push(format!("#{key} (local id revoked)")),
            }
        }
        v
    };
    for username in usernames {
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
        let msg = format!(
            "Cannot resolve user device: {}\nMake sure their account is registered in the directory.",
            failed.join(", ")
        );
        eprintln!("[create_group_flow] {msg}");
        show_app_error(weak.clone(), String::from("Cannot resolve user device"));
        return false;
    }
    if !failed.is_empty() {
        eprintln!(
            "[create_group_flow] some members failed: {} (continuing with {} resolved)",
            failed.join(", "),
            members.len()
        );
    }
    if members.len() >= chatx_core::GROUP_P2P_MAX {
        tracing::warn!(
            count = members.len(),
            max = chatx_core::GROUP_P2P_MAX,
            "Group size hits the P2P cap (pair count O(N²); reachability risk rises sharply); enabling server fan-out plan B"
        );
    }

    let group_id = format!(
        "grp-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let title = format!("Group ({})", members.len());
    let use_b = should_use_server_fanout(members.len());
    let create_ok = if use_b {
        client.create_server_group(&group_id, members).map(|_| ())
    } else {
        client
            .create_group(&group_id, members.clone())
            .and_then(|_| {
                client.announce_group(&group_id)?;
                client.publish_group_key(&group_id)?;
                Ok(())
            })
    };
    match create_ok {
        Ok(_) => {
            let new_key = {
                let mut g = backend.blocking_write();
                g.append_chat(group_id.clone(), title, true, String::new())
            };
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    let state = ui.global::<AppState>();
                    state.set_group_status(SharedString::new());
                    publish_to_views(&state, backend.clone());
                    load_chat_messages(ui.as_weak(), new_key);
                    push_sub_history(
                        &state,
                        SubPageEntry {
                            page: SubPageType::ChatRoom,
                            payload: new_key,
                        },
                    );
                }
            });
            true
        }
        Err(e) => {
            let msg = "Failed to create group".to_string();
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

                eprintln!("[search] NOT FOUND: {q2}");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<AppState>().set_add_status(SharedString::from(format!("User not found: {q2}")));
                    }
                });
            });
        }
        _ => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppState>()
                        .set_add_status(SharedString::from("Not signed in or backend not ready"));
                }
            });
        }
    }
}

fn search_users_by_peer_id(weak: slint::Weak<MainWindow>, q_raw: String) {
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
                        ui.global::<AppState>().set_add_status(SharedString::from(format!("User not found: {q2}")));
                    }
                });
            });
        }
        _ => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppState>()
                        .set_add_status(SharedString::from("Not signed in or backend not ready"));
                }
            });
        }
    }
}

fn add_contact(weak: slint::Weak<MainWindow>, username_raw: String, peer_id_raw: String) {
    let username: String = username_raw
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let peer_id: String = peer_id_raw.chars().filter(|c| !c.is_whitespace()).collect();
    if username.is_empty() && peer_id.is_empty() {
        return;
    }

    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let client = CLIENT.lock().unwrap().clone();
    let (Some(backend), Some(pool), Some(client)) = (backend.clone(), pool, client) else {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>()
                    .set_add_status(SharedString::from("Not signed in or backend not ready"));
            }
        });
        return;
    };

    let me = current_me();
    let me_peer = client.peer_base58();

    if !me.is_empty() && username.eq_ignore_ascii_case(&me) {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>()
                    .set_add_status(SharedString::from("Can't add yourself"));
            }
        });
        return;
    }

    // Already friends — nothing to do.
    if !peer_id.is_empty() && backend.blocking_read().has_contact(&peer_id, &username) {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>()
                    .set_add_status(SharedString::from("Already in your contacts"));
            }
        });
        return;
    }

    // The target must be reachable over P2P for the request to arrive.
    if peer_id.is_empty() {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<AppState>().set_add_status(SharedString::from(
                    "Cannot fetch the other party's device info",
                ));
            }
        });
        return;
    }

    let rt = runtime();
    let frame = FriendQ::new(FriendQAction::Inquire, &me_peer, &me, &peer_id)
        .encode()
        .unwrap_or_default();

    rt.spawn(async move {
        if let Err(e) = client.send_friendq(&peer_id, &frame).await {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<AppState>()
                        .set_add_status(SharedString::from(format!("Send failed: {e}")));
                }
            });
            return;
        }
        // Persist now that delivery succeeded; note carries the username for
        // later rendering even if it is never resolved locally.
        let _ = sqlite::social::frq_upsert(&pool, &me_peer, &peer_id, Some(&username)).await;
        {
            let mut g = backend.write().await;
            g.add_friend_request(
                &peer_id,
                username.clone(),
                sqlite::now_ms(),
                data::FrqDir::Sent,
            );
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let state = ui.global::<AppState>();
                publish_to_views(&state, backend.clone());
                state.set_add_status(SharedString::from(
                    "Request sent. Waiting for the other party to approve.",
                ));
            }
        });
    });
}

/// Create a contact entry for `their_peer` (a libp2p peer id), using `name`
/// (the other party's display name / username). Called on accept of a friend
/// request or after a legacy direct add.
async fn do_add(
    pool: &sqlx::SqlitePool,
    me: &str,
    their_peer: &str,
    name: &str,
    backend: &ArcBackend,
) -> anyhow::Result<()> {
    let me_id = sqlite::users::get_by_username(pool, me)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no user row for {me}; run login to create one"))?
        .id;
    // Ensure their user + device rows exist locally so future messaging,
    // contact lookups, etc. work without a live directory.
    let their_id = match sqlite::users::get_by_username(pool, name).await? {
        Some(u) => u.id,
        None => {
            sqlite::users::upsert(
                pool,
                sqlite::new_id(),
                &sqlite::users::UserPatch {
                    username: Some(name.to_string()),
                    nickname: Some(name.to_string()),
                    ..Default::default()
                },
            )
            .await?
            .id
        }
    };
    if their_id == me_id {
        anyhow::bail!("Can't add yourself");
    }
    sqlite::social::add(pool, me_id, their_id).await?;
    if !their_peer.is_empty() {
        let _ = sqlite::devices::bind_device(pool, their_id, their_peer).await;
    }

    let mut g = backend.write().await;
    if !g.has_contact(their_peer, name) {
        g.append_contact(
            their_id,
            their_peer.to_string(),
            name.to_string(),
            String::new(),
        );
    }
    Ok(())
}

fn accept_friend_request(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend.clone(), pool) else {
        return;
    };

    let (dir, other_peer) = match backend.blocking_read().frq_for(key) {
        Some((dir, peer)) => (dir, peer.to_string()),
        None => return,
    };
    if dir != data::FrqDir::Inbox {
        return; // only the inbox can be accepted here.
    }

    let me = current_me();
    let me_peer = {
        let c = CLIENT.lock().unwrap().clone();
        match c {
            Some(c) => c.peer_base58(),
            None => return,
        }
    };

    // Send the accept back to the requester (best-effort; they may be offline).
    if let Some(client) = CLIENT.lock().unwrap().clone() {
        if let Some(frame) =
            FriendQ::new(FriendQAction::Accept, &me_peer, &me, &other_peer).encode()
        {
            let peer = other_peer.clone();
            let weak2 = weak.clone();
            let rt = runtime();
            rt.spawn(async move {
                let _ = client.send_friendq(&peer, &frame).await;
                let _ = weak2;
            });
        }
    }

    let rt = runtime();
    rt.spawn(async move {
        // 1. Persist the acceptance in the local DB.
        let _ = sqlite::social::frq_set_status(
            &pool,
            &other_peer,
            &me_peer,
            sqlite::social::FRQ_ACCEPTED,
        )
        .await;

        // 2. Resolve a friendly name and create the contact entry (idempotent).
        let display = {
            let b = backend.read().await;
            b.friend_requests
                .iter()
                .find(|r| r.id == key)
                .map(|r| r.name.clone())
                .unwrap_or_else(|| other_peer.clone())
        };
        let name = if display == other_peer {
            data::name_for_peer(&pool, &other_peer).await
        } else {
            display
        };
        if let Err(e) = do_add(&pool, &me, &other_peer, &name, &backend).await {
            eprintln!("[friendq] accept failed: {e}");
        }

        // 3. Refresh UI.
        {
            let mut g = backend.write().await;
            g.remove_friend_request(key);
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let state = ui.global::<AppState>();
                publish_to_views(&state, backend.clone());
                state.set_notice_text(SharedString::from(format!("{name} joined your contacts")));
            }
        });
    });
}

fn reject_friend_request(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend.clone(), pool) else {
        return;
    };

    let (dir, other_peer) = match backend.blocking_read().frq_for(key) {
        Some((dir, peer)) => (dir, peer.to_string()),
        None => return,
    };
    if dir != data::FrqDir::Inbox {
        return;
    }

    let me_peer = match CLIENT.lock().unwrap().clone() {
        Some(c) => c.peer_base58(),
        None => return,
    };
    let me_name = current_me();

    if let Some(client) = CLIENT.lock().unwrap().clone() {
        if let Some(frame) =
            FriendQ::new(FriendQAction::Reject, &me_peer, &me_name, &other_peer).encode()
        {
            let peer = other_peer.clone();
            let rt = runtime();
            rt.spawn(async move {
                let _ = client.send_friendq(&peer, &frame).await;
            });
        }
    }

    let rt = runtime();
    rt.spawn(async move {
        let _ = sqlite::social::frq_set_status(
            &pool,
            &other_peer,
            &me_peer,
            sqlite::social::FRQ_REJECTED,
        )
        .await;
        {
            let mut g = backend.write().await;
            g.remove_friend_request(key);
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let state = ui.global::<AppState>();
                publish_to_views(&state, backend.clone());
            }
        });
    });
}

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
            label: SharedString::from(if d.label.is_empty() {
                d.peer_id.clone()
            } else {
                d.label
            }),
            online: now - d.seen as i64 <= chatx_core::signal::ONLINE_TTL_MS as i64,
            approved: d.status == chatx_core::account::DeviceStatus::Approved,
            self_device: d.peer_id == me_peer,
        })
        .collect();

    let approved = list
        .iter()
        .find(|d| d.peer_id.to_string() == me_peer)
        .map(|d| d.approved)
        .unwrap_or(false);
    DEVICE_APPROVED.with(|c| c.set(approved));

    let notice = if approved {
        String::new()
    } else {
        "Device pending approval · have an approved device approve it to send/receive".to_string()
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
            show_app_notice(weak, "This device was approved".to_string(), true);
        }
        Err(e) => {
            eprintln!("[approve] device {peer} failed: {e}");
            show_app_error(weak, format!("Failed to approve: {e}"));
        }
    }
}

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
            show_app_notice(weak, "This device was revoked".to_string(), true);
        }
        Err(e) => {
            eprintln!("[revoke] device {peer} failed: {e}");
            show_app_error(weak, format!("Failed to revoke: {e}"));
        }
    }
}

fn require_approved(weak: slint::Weak<MainWindow>) -> bool {
    if device_is_approved() {
        return true;
    }
    show_app_error(
        weak,
        "This device is pending approval. Approve it from an approved device first.".to_string(),
    );
    false
}

fn show_app_error(weak: slint::Weak<MainWindow>, msg: String) {
    show_app_notice(weak, msg, false);
}

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

/// Reset `CallState` to a clean active / inactive baseline.
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
            cs.set_connected(false);
            cs.set_muted(false);
            cs.set_audio_muted(call::is_remote_muted());
            // Reset device state — callers set these again after a
            // successful `start_call` + `toggle_my_video` (for video).
            cs.set_cam_on(false);
            cs.set_scr_on(false);
            // Reset local image sources + "has-*" flags so the surfaces and
            // device buttons start gray; they come back on when each local
            // camera / screen frame is pushed + `has_my_*` is set.
            cs.set_my_cam(slint::Image::default());
            cs.set_my_scr(slint::Image::default());
            cs.set_has_my_cam(false);
            cs.set_has_my_scr(false);
            cs.set_has_remote_cam(false);
            cs.set_has_remote_scr(false);
            cs.set_swapped(false);
            cs.set_minimized(false);
            cs.set_error_message(SharedString::new());
            if let Some(p) = peer_base58 {
                cs.set_peer_base58(SharedString::from(p));
            }
            if let Some(n) = peer_name {
                cs.set_peer_name(SharedString::from(n));
            }
        }
    });
}

/// Resolve a 1:1 chat row key to its peer's contact-list key (or -1 for
/// group chats / unknown peers). The key is what `ProfileState.start-*`
/// callbacks consume.
fn chat_key_to_contact_key(chat_key: i32) -> i32 {
    let backend = BACKEND.lock().unwrap().clone();
    let Some(backend) = backend else {
        return -1;
    };
    let me_peer = CLIENT
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.peer_base58().to_string());
    let g = backend.blocking_read();
    let Some(row) = g.chats.iter().find(|r| r.key == chat_key) else {
        return -1;
    };
    if row.is_group || row.peer_id.is_empty() {
        return -1;
    }
    let peer_id = row.peer_id.clone();
    drop(g);
    let g = backend.blocking_read();
    g.contacts
        .iter()
        .find(|c| {
            c.peer_id == peer_id && me_peer.as_deref().map(|m| m != c.peer_id).unwrap_or(true)
        })
        .map(|c| c.key)
        .unwrap_or(-1)
}

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
        show_app_error(
            weak,
            String::from("Cannot start a call: not signed in or backend not ready"),
        );
        return;
    };
    let client = Arc::new(client);

    let (peer_base58, peer_name) = {
        let g = backend.blocking_read();
        let Some(pid) = g.peer_id_for(key).map(|s| s.to_string()) else {
            eprintln!("[audio.call] begin_voice_call: no peer_id for key={key} — abort");
            show_app_error(
                weak,
                format!("Contact is invalid (key={key}); cannot start a call"),
            );
            return;
        };
        let name = g
            .contacts
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| pid.clone());
        eprintln!("[audio.call] begin_voice_call: resolving peer_base58={pid} name={name}");
        (pid, name)
    };

    eprintln!("[audio.call] begin_voice_call: about to call::start_call");
    match call::start_call(
        Arc::clone(&client),
        &peer_base58,
        call::CallOptions {
            include_cam: false,
            include_scr: false,
        },
    ) {
        Ok(()) => {
            eprintln!("[audio.call] begin_voice_call: start_call OK — showing AudioCall overlay");
            publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
            show_call_overlay(weak, GlobalOverlayType::AudioCall);
        }
        Err(e) => {
            eprintln!("[audio.call] begin_voice_call: start_call FAILED: {e}");
            show_app_error(weak, format!("Failed to start call: {e}"));
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
        let name = g
            .contacts
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| pid.clone());
        (pid, name)
    };

    if let Err(e) = call::start_call(
        Arc::clone(&client),
        &peer_base58,
        call::CallOptions {
            include_cam: true,
            include_scr: false,
        },
    ) {
        publish_call_error(weak, format!("Failed to start video call: {e}"));
        return;
    }
    publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
    // Cam is on for this call (it was attached in the initial offer).
    let wk = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = wk.upgrade() {
            ui.global::<CallState>().set_cam_on(true);
        }
    });
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
        let name = g
            .contacts
            .iter()
            .find(|r| r.key == key)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| pid.clone());
        (pid, name)
    };

    // `start_call` → `bootstrap_media` → `start_local_capture(Scr)` starts
    // the capture device (xcap / MediaProjection / ReplayKit) and wires the
    // dispatcher. We don't start it here.
    if let Err(e) = call::start_call(
        Arc::clone(&client),
        &peer_base58,
        call::CallOptions {
            include_cam: false,
            include_scr: true,
        },
    ) {
        publish_call_error(weak, format!("Failed to start screen share: {e}"));
        return;
    }
    publish_call_state(weak.clone(), Some(peer_base58), Some(peer_name), true);
    let wk = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = wk.upgrade() {
            ui.global::<CallState>().set_scr_on(true);
        }
    });
    show_call_overlay(weak, GlobalOverlayType::ScreenShare);
}

/// Handle a mid-call CAMERA / SCREEN on-off toggle (one of the four always-
/// on UI buttons). Enforces mutual exclusivity (CAMERA and SCREEN cannot
/// both be on at once — the new button the user tapped wins, and the other
/// device is stopped first). Also:
///   * On-attach: starts the local device, builds a `VidSource`, attaches it
///     to the active call via `PeerCall::upgrade_offer`, sends a
///     `Cam(true)` / `Screen(true)` control signal so the *peer* updates
///     their UI, and installs the UI preview sink.
///   * On-detach: stops the local device, mutes the `VidSource` (last frame
///     stays on the peer's side), sends `Cam(false)` / `Screen(false)`,
///     clears the local preview sink, and resets the local "has-my-*" flag.
fn toggle_my_video(weak: slint::Weak<MainWindow>, kind: media::VKind) {
    // Read the target state from the UI.
    let target_on = match weak.upgrade() {
        Some(ui) => {
            let st = ui.global::<CallState>();
            if kind == media::VKind::Cam {
                !st.get_cam_on()
            } else {
                !st.get_scr_on()
            }
        }
        None => return,
    };

    if target_on {
        // Enforce exclusivity: if the *other* local device is on, stop it
        // first so we never have two local video tracks attached.
        let other = if kind == media::VKind::Cam {
            media::VKind::Scr
        } else {
            media::VKind::Cam
        };
        let other_on = match weak.upgrade() {
            Some(ui) => {
                let st = ui.global::<CallState>();
                if other == media::VKind::Scr {
                    st.get_scr_on()
                } else {
                    st.get_cam_on()
                }
            }
            None => return,
        };
        if other_on {
            // Detach the other one first (stop device, mute source — no
            // control signal; internal state reconciliation). Keep the other
            // source alive for reuse (don't stop its encoder); just detach it
            // from the webrtc slot and release its last frame.
            stop_local_device(other);
            if let Some(c) = call::active_call() {
                c.set_video_mute(other, true);
                if let Some(s) = c.local_source(other) {
                    s.clear();
                }
            }
            detach_video_source_kept(other);
            match weak.upgrade() {
                Some(ui) => {
                    let st = ui.global::<CallState>();
                    if other == media::VKind::Cam {
                        st.set_cam_on(false);
                        st.set_my_cam(slint::Image::default());
                        st.set_has_my_cam(false);
                    } else {
                        st.set_scr_on(false);
                        st.set_my_scr(slint::Image::default());
                        st.set_has_my_scr(false);
                    }
                }
                None => return,
            }
        }

        // Attach the new source. Reuse this call's existing source for the
        // kind if we already have one (the `Arc` is pinned in
        // `PeerCall.inner.cam/scr`); only allocate a fresh `VidSource` on the
        // first attach. Reusing keeps the number of live sources bounded (one
        // per kind) instead of leaking a new one (openh264 encoder + full-res
        // frame + webrtc track binding) on every toggle.
        start_local_device(kind);
        let src = match call::active_call().and_then(|c| c.local_source(kind)) {
            Some(s) => s,
            None => match capture_source(kind) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[call] toggle_video: capture_source({:?}): {e}", kind);
                    return;
                }
            },
        };
        // Make sure the capture dispatcher forwards frames to this source.
        match kind {
            media::VKind::Cam => call_dispatcher::set_cam_source(Some(std::sync::Arc::clone(&src))),
            media::VKind::Scr => call_dispatcher::set_scr_source(Some(std::sync::Arc::clone(&src))),
        }
        // Re-enabling a kind that was previously muted (on its last detach)
        // — unmute the reused source so frames actually encode/transmit again.
        if let Some(c) = call::active_call() {
            c.set_video_mute(kind, false);
        }
        if let Err(e) = call::upgrade_outgoing(kind, false, src) {
            eprintln!("[call] toggle_video: upgrade_outgoing({:?}): {e}", kind);
            // `upgrade_outgoing` moved/recorded `src` into `PeerCall.inner`;
            // just detach the dispatcher slot. Keep the source alive (no
            // stop) so a retry can reuse it.
            detach_video_source_kept(kind);
            return;
        }
        // Tell the peer that our cam / screen is now ON (so their UI flips
        // `has-remote-cam`/`has-remote-scr`).
        if let Some(c) = call::active_call() {
            c.send_control(match kind {
                media::VKind::Cam => media::SignalKind::Cam(true),
                media::VKind::Scr => media::SignalKind::Screen(true),
            });
        }
        // Install the local preview sink so the A slot shows *me*.
        match kind {
            media::VKind::Cam => call::install_local_cam_preview(),
            media::VKind::Scr => call::install_local_scr_preview(),
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<CallState>();
                if kind == media::VKind::Cam {
                    st.set_cam_on(true);
                } else {
                    st.set_scr_on(true);
                }
            }
        });
    } else {
        // Detach: stop the device, mute the source, tell the peer, release
        // the last frame. The `VidSource` itself stays alive (its
        // `Arc` is in `PeerCall.inner.{cam,scr}`) so it can be reused on the
        // next attach — no new encoder / track binding allocated.
        stop_local_device(kind);
        if let Some(c) = call::active_call() {
            c.set_video_mute(kind, true);
            if let Some(s) = c.local_source(kind) {
                s.clear();
            }
            c.send_control(match kind {
                media::VKind::Cam => media::SignalKind::Cam(false),
                media::VKind::Scr => media::SignalKind::Screen(false),
            });
        }
        // Detach the dispatcher slot (device is already stopped, so no more
        // frames will arrive). Do NOT stop the encoder: we want to reuse
        // this source later.
        detach_video_source_kept(kind);
        // Stop forwarding frames to the UI preview sink.
        match kind {
            media::VKind::Cam => call_dispatcher::set_cam_preview(None),
            media::VKind::Scr => call_dispatcher::set_scr_preview(None),
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let st = ui.global::<CallState>();
                if kind == media::VKind::Cam {
                    st.set_cam_on(false);
                    st.set_my_cam(slint::Image::default());
                    st.set_has_my_cam(false);
                } else {
                    st.set_scr_on(false);
                    st.set_my_scr(slint::Image::default());
                    st.set_has_my_scr(false);
                }
            }
        });
    }
}

/// Start the local hardware capture device (`VKind`) **only**. Does NOT
/// create a `VidSource` or register it in the dispatcher — call
/// [`capture_source`] separately once you need an outbound track. Idempotent.
/// Must be called on the UI thread (it mutates the `CAMERA` / `SCREEN`
/// thread_locals).
pub(crate) fn start_local_device(kind: media::VKind) {
    match kind {
        media::VKind::Cam => {
            CAMERA.with(|slot| {
                if slot.borrow().is_none() {
                    *slot.borrow_mut() = Some(Camera::new());
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
        }
        media::VKind::Scr => {
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
                        if let Err(e) = scr.start() {
                            eprintln!("[screen] start failed: {e}");
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
        }
    }
}

/// Build a fresh outbound `VidSource` for `VKind` and register it in the
/// call dispatcher (so the platform capture sink's frames flow into it).
/// Callers then attach the returned source to their active `PeerCall`
/// (either via `PeerCall::upgrade_offer` / `upgrade_answer` during a
/// mid-call upgrade, or via `PeerCall::offer`/`answer` when starting one).
pub(crate) fn capture_source(
    kind: media::VKind,
) -> anyhow::Result<std::sync::Arc<media::VidSource>> {
    let src = std::sync::Arc::new(
        media::VidSource::new(kind).map_err(|e| anyhow::anyhow!("VidSource::new: {e}"))?,
    );
    match kind {
        media::VKind::Cam => call_dispatcher::set_cam_source(Some(std::sync::Arc::clone(&src))),
        media::VKind::Scr => call_dispatcher::set_scr_source(Some(std::sync::Arc::clone(&src))),
    }
    Ok(src)
}

/// Detach a `VidSource` from the webrtc dispatcher slot WITHOUT stopping it,
/// so the source (encoder thread + last frame + track binding) can be
/// reused on the next attach. The `Arc` stays alive via
/// `PeerCall.inner.cam/scr` (and `CallInner`), so nothing is dropped.
///
/// This is what mid-call toggles use: switching between cam/scr must not
/// allocate a fresh `VidSource` each time — old ones would accumulate
/// (openh264 native encoders, full-res frames, webrtc track bindings), which
/// is the source of the memory climb. `stop()` (encoder exit) still happens
/// on hangup via `PeerCall::close` / end-call.
fn detach_video_source_kept(kind: media::VKind) {
    match kind {
        media::VKind::Cam => {
            let _ = call_dispatcher::take_cam_source();
        }
        media::VKind::Scr => {
            let _ = call_dispatcher::take_scr_source();
        }
    }
}

/// Convenience: start the device AND its matching `VidSource` in one call.
/// Reused by `begin_video_call`-style bootstrap AND mid-call upgrades.
pub(crate) fn start_local_capture(
    kind: media::VKind,
) -> anyhow::Result<std::sync::Arc<media::VidSource>> {
    start_local_device(kind);
    capture_source(kind)
}

/// Stop just one local capture device (`Cam` or `Scr`). Used by the
/// mid-call CAMERA / SCREEN button to release the hardware without touching
/// the other device.
pub(crate) fn stop_local_device(kind: media::VKind) {
    match kind {
        media::VKind::Cam => {
            CAMERA.with(|slot| {
                if let Some(cam) = slot.borrow_mut().as_mut() {
                    if cam.is_running() {
                        cam.stop();
                    }
                }
            });
        }
        media::VKind::Scr => {
            #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
            {
                SCREEN.with(|slot| {
                    if let Some(s) = slot.borrow_mut().as_mut() {
                        if s.is_running() {
                            s.stop();
                        }
                    }
                });
            }
            #[cfg(target_os = "android")]
            {
                bridge::request_screen_share_stop();
            }
            #[cfg(target_os = "ios")]
            {
                bridge::request_screen_share_stop();
            }
        }
    }
}

/// Release both local capture devices. Called on `stop_call`.
pub(crate) fn stop_local_media_capture() {
    stop_local_device(media::VKind::Cam);
    stop_local_device(media::VKind::Scr);
}

fn end_screen_share(weak: slint::Weak<MainWindow>) {
    stop_local_media_capture();
    show_call_overlay(weak, GlobalOverlayType::None);
}

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
        publish_profile_status(
            weak,
            key,
            SharedString::from("Not signed in or backend not ready"),
        );
        return;
    };
    let Some(user_id) = backend.blocking_read().contact_id_for(key) else {
        publish_profile_status(weak, key, SharedString::from("Contact is no longer valid"));
        return;
    };
    let peer_id = backend
        .blocking_read()
        .peer_id_for(key)
        .map(|s| s.to_string())
        .unwrap_or_default();

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
                    publish_profile_status(
                        weak2,
                        my_key,
                        SharedString::from("This user is not synced on this device"),
                    );
                }
                return;
            }
            Err(e) => {
                if PROFILE_LATEST.load(std::sync::atomic::Ordering::SeqCst) == seq {
                    publish_profile_status(
                        weak2,
                        my_key,
                        SharedString::from(format!("Read failed: {e}")),
                    );
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

fn open_conversation_with_contact(weak: slint::Weak<MainWindow>, key: i32) {
    let t0 = std::time::Instant::now();
    eprintln!("[chat] open_conversation_with_contact: enter key={key}");
    let client = CLIENT.lock().unwrap().clone();
    eprintln!(
        "[chat] open_conversation: +{}ms grabbed CLIENT",
        t0.elapsed().as_millis()
    );
    let backend = BACKEND.lock().unwrap().clone();
    eprintln!(
        "[chat] open_conversation: +{}ms grabbed BACKEND",
        t0.elapsed().as_millis()
    );
    let (Some(client), Some(backend)) = (client, backend) else {
        show_app_error(weak, format!("Not signed in or backend not ready"));
        return;
    };
    let row_clone = {
        let g = backend.blocking_read();
        let row = g.contacts.iter().find(|r| r.key == key).cloned();
        drop(g);
        row
    };
    eprintln!(
        "[chat] open_conversation: +{}ms backend row_clone ok={}",
        t0.elapsed().as_millis(),
        row_clone.is_some()
    );
    let Some(c) = row_clone else {
        show_app_error(weak, format!("Contact is no longer valid"));
        return;
    };
    if c.peer_id.is_empty() {
        show_app_error(
            weak,
            format!("Contact has no peer id; cannot start a conversation"),
        );
        return;
    }
    let peer_id = c.peer_id;
    let title = c.name;
    eprintln!(
        "[chat] open_conversation: +{}ms peer_id={peer_id}",
        t0.elapsed().as_millis()
    );
    let me_peer = client.peer_base58();
    eprintln!(
        "[chat] open_conversation: +{}ms self peer={me_peer}",
        t0.elapsed().as_millis()
    );
    let chat_id = dm_chat_id(&me_peer, &peer_id);
    eprintln!(
        "[chat] open_conversation: +{}ms chat_id={chat_id}",
        t0.elapsed().as_millis()
    );
    eprintln!(
        "[chat] open_conversation: +{}ms THREAD {:?} about to acquire READ lock (2nd time, after row_clone)",
        t0.elapsed().as_millis(),
        std::thread::current().id()
    );
    let existing_key = backend
        .blocking_read()
        .chats
        .iter()
        .find(|c| c.chat_id == chat_id)
        .map(|c| c.key);
    eprintln!(
        "[chat] open_conversation: +{}ms THREAD {:?} READ lock released, existing={:?}",
        t0.elapsed().as_millis(),
        std::thread::current().id(),
        existing_key
    );
    let new_key = match existing_key {
        Some(k) => k,
        None => {
            eprintln!(
                "[chat] open_conversation: +{}ms THREAD {:?} about to acquire WRITE lock (append_chat)",
                t0.elapsed().as_millis(),
                std::thread::current().id()
            );
            let mut g = backend.blocking_write();
            let k = g.append_chat(chat_id, title, false, peer_id.clone());
            drop(g);
            eprintln!(
                "[chat] open_conversation: +{}ms THREAD {:?} WRITE lock released, appended new chat key={k}",
                t0.elapsed().as_millis(),
                std::thread::current().id()
            );
            k
        }
    };
    eprintln!(
        "[chat] open_conversation: +{}ms new_key={new_key}, about to invoke_from_event_loop",
        t0.elapsed().as_millis()
    );
    let _ = slint::invoke_from_event_loop(move || {
        eprintln!(
            "[chat] open_conversation: +{}ms eventloop cb enter",
            t0.elapsed().as_millis()
        );
        let Some(ui) = weak.upgrade() else {
            eprintln!(
                "[chat] open_conversation: +{}ms weak upgrade failed",
                t0.elapsed().as_millis()
            );
            return;
        };
        eprintln!("[chat] open_conversation: eventloop cb calling publish_chat_rows");
        let state = ui.global::<AppState>();
        publish_chat_rows(&state, backend.clone());
        eprintln!(
            "[chat] open_conversation: eventloop cb publish_chat_rows done, calling load_chat_messages"
        );
        load_chat_messages(ui.as_weak(), new_key);
        eprintln!(
            "[chat] open_conversation: eventloop cb load_chat_messages spawned, calling push_sub_history"
        );
        {
            let mut nav = state.get_nav_state();
            if nav.active_tab != 0 {
                nav.active_tab = 0;
                state.set_nav_state(nav);
            }
        }
        push_sub_history(
            &state,
            SubPageEntry {
                page: SubPageType::ChatRoom,
                payload: new_key,
            },
        );
        eprintln!(
            "[chat] open_conversation: +{}ms eventloop cb done",
            t0.elapsed().as_millis()
        );
    });
    eprintln!(
        "[chat] open_conversation: +{}ms invoke_from_event_loop queued (not yet run)",
        t0.elapsed().as_millis()
    );
}

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
    if !t.starts_with('[') {
        return vec![t.to_string()];
    }
    Vec::new()
}

fn load_post_detail(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(
            weak,
            key,
            SharedString::from("Not signed in or backend not ready"),
        );
        return;
    };
    let Some(post_id) = detail_key_post(&backend, key) else {
        publish_detail_status(weak, key, SharedString::from("Post is no longer available"));
        return;
    };

    let me = current_me();
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
                    publish_detail_status(weak2, key, SharedString::from("Post does not exist"));
                }
                return;
            }
        };

        let me_id = sqlite::users::get_by_username(&pool, &me)
            .await
            .ok()
            .flatten()
            .map(|u| u.id)
            .unwrap_or(0);
        let i_liked = sqlite::social_feed::has_liked(&pool, post_id, me_id)
            .await
            .unwrap_or(false);
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
                format!("{} and {} more liked this", head, likers.len())
            } else {
                format!("{} liked this", head)
            }
        };

        let comments = match sqlite::joins::post_comments(&pool, post_id, 200, 0).await {
            Ok(rows) => rows,
            Err(e) => {
                if POST_LATEST.load(std::sync::atomic::Ordering::SeqCst) == seq {
                    publish_detail_status(
                        weak2,
                        key,
                        SharedString::from(format!("Failed to read comments: {e}")),
                    );
                }
                return;
            }
        };

        let media_urls = match post.media_urls.clone() {
            Some(s) if !s.trim().is_empty() => parse_media_urls(&s),
            _ => Vec::new(),
        };

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

fn toggle_detail_like(weak: slint::Weak<MainWindow>, key: i32) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(
            weak,
            key,
            SharedString::from("Not signed in or backend not ready"),
        );
        return;
    };
    let post_id = detail_key_post(&backend, key);
    let (want_liked, likes) = {
        let mut g = backend.blocking_write();
        let (liked, likes) = g.toggle_like(key);
        (liked, likes)
    };
    if let Some(ui) = weak.upgrade() {
        publish_to_views(&ui.global::<AppState>(), backend.clone());
        let ps = ui.global::<PostDetailState>();
        ps.set_liked(want_liked);
        ps.set_likes(likes as i32);
    }
    if let Some(post_id) = post_id {
        let rt = runtime();
        let me = current_me();
        rt.spawn(async move {
            let _ = data::persist_like_toggle(&pool, &me, post_id, want_liked).await;
        });
    }
}

fn add_post_comment(weak: slint::Weak<MainWindow>, key: i32, body: String) {
    let backend = BACKEND.lock().unwrap().clone();
    let pool = POOL.lock().unwrap().clone();
    let (Some(backend), Some(pool)) = (backend, pool) else {
        publish_detail_status(
            weak,
            key,
            SharedString::from("Not signed in or backend not ready"),
        );
        return;
    };
    let Some(post_id) = detail_key_post(&backend, key) else {
        publish_detail_status(weak, key, SharedString::from("Post is no longer available"));
        return;
    };
    let me = CLIENT
        .lock()
        .unwrap()
        .as_ref()
        .map(|c| c.user_id().to_string())
        .unwrap_or_default();
    let my_name = {
        let n = weak
            .clone()
            .upgrade()
            .map(|ui| ui.global::<AppState>().get_nickname().to_string())
            .unwrap_or_default();
        if n.is_empty() { me.clone() } else { n }
    };
    let rt = runtime();
    rt.spawn(async move {
        let me_id = match sqlite::users::get_by_username(&pool, &me).await {
            Ok(Some(u)) => u.id,
            Ok(None) => {
                publish_detail_status(weak.clone(), key, SharedString::from("Comment failed"));
                return;
            }
            Err(e) => {
                publish_detail_status(
                    weak.clone(),
                    key,
                    SharedString::from(format!("Comment failed: {e}")),
                );
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
                        time_label: SharedString::from("just now"),
                    };
                    list.insert(0, new);
                    ps.set_comment_list(slint::ModelRc::new(slint::VecModel::from(list)));
                    ps.set_comments((ps.get_comments() as i64 + 1) as i32);
                    ps.set_status(SharedString::new());
                });
            }
            Err(e) => {
                publish_detail_status(
                    weak,
                    key,
                    SharedString::from(format!("Comment failed: {e}")),
                );
            }
        }
    });
}
