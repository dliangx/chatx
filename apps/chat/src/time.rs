use chrono::TimeZone;
use std::sync::{OnceLock, RwLock};

fn local_datetime(ms: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::Local
        .timestamp_opt(ms / 1000, (ms % 1000) as u32 * 1_000_000)
        .single()
}

// ── Language bridge ───────────────────────────────────────────────
// `lib.rs` mirrors the language it hands to Slint into this cell; the
// five "X ago" labels then resolve against the same catalog on the fly.
static LANG: OnceLock<RwLock<String>> = OnceLock::new();

pub fn set_lang(code: &str) {
    let cell = LANG.get_or_init(|| RwLock::new(String::new()));
    let mut g = cell.write().unwrap();
    *g = code.to_string();
}

#[derive(Copy, Clone)]
struct L10n {
    just_now: &'static str,
    min_ago: &'static str,
    h_ago: &'static str,
    d_ago: &'static str,
    y_ago: &'static str,
}

const EN: L10n = L10n {
    just_now: "just now",
    min_ago: "{} min ago",
    h_ago: "{} hours ago",
    d_ago: "{} days ago",
    y_ago: "{} years ago",
};
const ZH_CN: L10n = L10n {
    just_now: "刚刚",
    min_ago: "{} 分钟前",
    h_ago: "{} 小时前",
    d_ago: "{} 天前",
    y_ago: "{} 年前",
};
const DE: L10n = L10n {
    just_now: "gerade eben",
    min_ago: "vor {} Min",
    h_ago: "vor {} Std",
    d_ago: "vor {} T",
    y_ago: "vor {} J",
};
const ES: L10n = L10n {
    just_now: "ahora mismo",
    min_ago: "hace {} min",
    h_ago: "hace {} h",
    d_ago: "hace {} d",
    y_ago: "hace {} a",
};
const FR: L10n = L10n {
    just_now: "à l'instant",
    min_ago: "il y a {} min",
    h_ago: "il y a {} h",
    d_ago: "il y a {} j",
    y_ago: "il y a {} a",
};
const JA: L10n = L10n {
    just_now: "たった今",
    min_ago: "{} 分前",
    h_ago: "{} 時間前",
    d_ago: "{} 日前",
    y_ago: "{} 年前",
};
const RU: L10n = L10n {
    just_now: "только что",
    min_ago: "{} мин назад",
    h_ago: "{} ч назад",
    d_ago: "{} дн назад",
    y_ago: "{} г назад",
};
const AR: L10n = L10n {
    just_now: "الآن",
    min_ago: "قبل {} دقيقة",
    h_ago: "قبل {} ساعة",
    d_ago: "قبل {} يوم",
    y_ago: "قبل {} سنة",
};

fn table(code: &str) -> &'static L10n {
    match code {
        "zh_CN" => &ZH_CN,
        "de" => &DE,
        "es" => &ES,
        "fr" => &FR,
        "ja" => &JA,
        "ru" => &RU,
        "ar" => &AR,
        _ => &EN,
    }
}

fn cur_l10n() -> &'static L10n {
    match LANG.get() {
        Some(cell) => {
            let g = cell.read().unwrap();
            table(g.as_str())
        }
        None => &EN,
    }
}

fn fill(tmpl: &str, n: i64) -> String {
    tmpl.replace("{}", &n.to_string())
}

// ── Public API ────────────────────────────────────────────────────

pub fn time_label(ms: i64) -> String {
    if ms <= 0 {
        return cur_l10n().just_now.into();
    }
    match local_datetime(ms) {
        Some(dt) => dt.format("%H:%M").to_string(),
        None => cur_l10n().just_now.into(),
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
    let t = cur_l10n();
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
