//! ChatX 桌面端入口。
//!
//! 启动流程：
//! 1. 主窗口（`MainWindow`）持有登录态属性（`logged-in` 等），驱动"登录页 / 主界面"。
//! 2. 登录/注册走 `p2pchat-core` 的 `Client::login` / `Client::bootstrap`（目录后端默认
//!    `HttpDirectory`，即中心化信号服务器）。
//! 3. `Client` 内持有 `!Send` 的事件接收端，必须与 UI 线程绑定（`thread_local!`），因此
//!    登录在 UI 线程用 `block_on` 执行（一次性网络/口令 KDF），`Client` 全程留在 UI 线程，
//!    不被跨线程移动。

use slint::SharedString;
use std::cell::RefCell;
use std::sync::Arc;

use chatx_core::Client;
use chatx_core::signal::HttpDirectory;

// 生成 Slint 组件 / 回调绑定（MainWindow、AuthView 等）。
slint::include_modules!();

type Directory = HttpDirectory;

// UI 线程专属的登录态容器（Client 为 !Send，不能进 static / 跨线程共享）。
thread_local! {
    static RUNTIME: RefCell<Option<Arc<tokio::runtime::Runtime>>> = RefCell::new(None);
    static CLIENT: RefCell<Option<Arc<Client<Directory>>>> = RefCell::new(None);
}

/// 惰性取出/创建一个多线程 tokio runtime，绑定到当前（UI）线程。
fn runtime() -> Arc<tokio::runtime::Runtime> {
    RUNTIME.with(|slot| {
        if slot.borrow().is_none() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("创建 tokio runtime 失败");
            *slot.borrow_mut() = Some(Arc::new(rt));
        }
        slot.borrow().as_ref().expect("runtime 已初始化").clone()
    })
}

/// 当前生效的 profile（`$P2PCHAT_PROFILE` 或 `default`）。
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
    let ui = MainWindow::new().expect("构建主窗口失败");
    let weak = ui.as_weak();

    // 启动时给出初始提示（本地是否已有账户），但不自动登录——真正的校验走登录按钮。
    ui.set_auth_message(if keystore_path(&profile()).exists() {
        SharedString::from("检测到本地已有账户，请输入口令登录")
    } else {
        SharedString::from("首次使用，请注册新账户")
    });

    // 登录：用口令解密已有 keystore 并启动设备（PENDING/APPROVED 由 core 判定）。
    {
        let weak = weak.clone();
        ui.on_login(move |user_id, pass| {
            let uid = user_id.to_string();
            let pass = pass.to_string();
            let profile = profile();
            let dir = default_server();
            let weak = weak.clone();
            // 立刻置忙，UI 立即反馈（按钮变"处理中…"）；KDF/HTTP 重活丢到后台线程，不冻结 UI。
            if let Some(ui) = weak.upgrade() {
                ui.set_auth_busy(true);
                ui.set_auth_message(SharedString::from("登录中…"));
            }
            let rt = runtime();
            rt.spawn(async move {
                let res = Client::login(&profile, &pass, uid.clone(), dir).await;
                // 事件循环已关闭（应用退出）时无从处理，忽略即可。
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_auth_busy(false);
                    }
                    apply_result(weak, uid, res);
                });
            });
        });
    }

    // 注册：无账户则创建并自批 APPROVED；有账户则校验 user_id/口令。
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
                ui.set_auth_message(SharedString::from("注册中…"));
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

    // 退出登录（清空内存中的 Client；keystore 仍留在磁盘）。
    ui.on_logout(move || {
        CLIENT.with(|s| *s.borrow_mut() = None);
        if let Some(ui) = weak.upgrade() {
            ui.set_logged_in(false);
            ui.set_auth_message(SharedString::from("已退出登录"));
        }
    });

    ui.run().expect("运行事件循环失败");
}

/// 把 bootstrap/login 的结果落到 UI：成功 → 保存 Client 并置 `logged-in`；失败 → 显示错误。
fn apply_result(
    weak: slint::Weak<MainWindow>,
    user_id: String,
    res: anyhow::Result<(Client<Directory>, chatx_core::account::Account)>,
) {
    match res {
        Ok((client, _acct)) => {
            let client = Arc::new(client);
            // 登录成功后做一次轻量心跳，让目录里本设备为"在线"。
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
        ui.set_auth_message(SharedString::from(format!("已登录：{user_id}")));
        ui.set_user_id(SharedString::from(user_id));
        ui.set_logged_in(true);
    }
}
