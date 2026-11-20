//! Relative/absolute time formatting for the `chat` app.
//!
//! The localized "X ago" strings and the "desktop-only" notice live in
//! [`crate::trans`] (the app's single translation catalog); this module only
//! does the date math against the current catalog from [`crate::trans::cur_l10n`].

use crate::trans;
use chrono::TimeZone;

fn local_datetime(ms: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::Local
        .timestamp_opt(ms / 1000, (ms % 1000) as u32 * 1_000_000)
        .single()
}

fn fill(tmpl: &str, n: i64) -> String {
    tmpl.replace("{}", &n.to_string())
}

// ── Public API ────────────────────────────────────────────────────

pub fn time_label(ms: i64) -> String {
    if ms <= 0 {
        return trans::cur_l10n().just_now.into();
    }
    match local_datetime(ms) {
        Some(dt) => dt.format("%H:%M").to_string(),
        None => trans::cur_l10n().just_now.into(),
    }
}

pub fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let now = chrono::Local::now();
    let dt = match local_datetime(ms) {
        Some(dt) => dt,
        None => return String::new(),
    };
    let diff = (now - dt).num_milliseconds();
    let diff = diff.max(0);
    let t = trans::cur_l10n();
    if diff < 60_000 {
        t.just_now.into()
    } else if diff < 3_600_000 {
        fill(t.min_ago, diff / 60_000)
    } else if diff < 86_400_000 {
        fill(t.h_ago, diff / 3_600_000)
    } else if diff < 86_400_000 * 365 {
        fill(t.d_ago, diff / 86_400_000)
    } else {
        fill(t.y_ago, diff / 86_400_000 / 365)
    }
}

pub fn fmt_profile_date(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    match local_datetime(ms) {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// The localized "documents are desktop-only" notice, for the app-layer
/// notice when a mobile build tries to open an Office document.
pub fn desktop_only_message() -> String {
    trans::cur_l10n().desktop_only.to_string()
}
