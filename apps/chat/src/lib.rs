use slint::SharedString;
use std::cell::RefCell;
use std::sync::Arc;

use chatx_core::Client;
use chatx_core::signal::HttpDirectory;

slint::include_modules!();

type Directory = HttpDirectory;

thread_local! {
    static RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> = RefCell::new(None);
    static CLIENT: RefCell<Option<Arc<Client<Directory>>>> = RefCell::new(None);
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
    ui.set_is_mobile(cfg!(target_os = "android") || cfg!(target_os = "ios"));
    // ui.set_is_mobile(true); 
    let state = ui.global::<AppState>();
    let weak = ui.as_weak();
    state.set_user_id(SharedString::from(""));
    let existing_uid = chatx_core::account::Keystore::load(&keystore_path(&profile()))
        .map(|ks| ks.user_id)
        .ok();
    if let Some(uid) = existing_uid {
        ui.set_auth_message(SharedString::from("please logging in "));
        state.set_user_id(SharedString::from(uid));
    }

    {
        let tabs: Vec<TabState> = (0..4).map(|_| TabState { sub_history: slint::ModelRc::new(slint::VecModel::from(Vec::<SubPageEntry>::new())) }).collect();
        let mut nav = state.get_nav_state();
        nav.tabs = slint::ModelRc::new(slint::VecModel::from(tabs));
        state.set_nav_state(nav);
    }
    {
        let w = weak.clone();
        state.on_push_sub(move |page, payload| {
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
        if let Some(ui) = w.upgrade() {
            ui.set_logged_in(false);
        }
    });

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
    let h = tab.sub_history.clone();
    let mut v2: Vec<SubPageEntry> = (0..h.row_count()).filter_map(|i| h.row_data(i)).collect();
    v2.pop();
    tab.sub_history = slint::ModelRc::new(slint::VecModel::from(v2));
    nav.tabs = slint::ModelRc::new(slint::VecModel::from(v));
    app.set_nav_state(nav);
}

fn apply_result(
    weak: slint::Weak<MainWindow>,
    user_id: String,
    res: anyhow::Result<(Client<Directory>, chatx_core::account::Account)>,
) {
    match res {
        Ok((client, _acct)) => {
            let client = Arc::new(client);
            client.heartbeat();
            CLIENT.with(|s| *s.borrow_mut() = Some(client));
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
