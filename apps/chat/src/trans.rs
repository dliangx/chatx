//! Translation catalogs + the app-language bridge for the `chat` app.
//!
//! This is the single home for all of the app's string catalogs:
//!   - the per-language `L10n` table used by the "X ago" time labels and the
//!     "desktop-only" notice (consumed by `time.rs`), and
//!   - the per-type inbound-notification bodies (`translate_notify_body`).
//!
//! `lib.rs` mirrors the language it hands to Slint into [`set_lang`] on every
//! language change; the catalogs below then resolve against it on the fly.
//! Reading the current language is thread-safe ([`get_lang`]).

use std::sync::{OnceLock, RwLock};

// ── Language bridge ───────────────────────────────────────────────
static LANG: OnceLock<RwLock<String>> = OnceLock::new();

pub fn set_lang(code: &str) {
    let cell = LANG.get_or_init(|| RwLock::new(String::new()));
    let mut g = cell.write().unwrap();
    *g = code.to_string();
}

/// Read the current app language (the value last passed to [`set_lang`]).
/// Returns `"en"` if no language has been set yet.
pub fn get_lang() -> String {
    LANG.get()
        .map(|c| c.read().unwrap().clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "en".to_string())
}

#[derive(Copy, Clone)]
pub struct L10n {
    pub just_now: &'static str,
    pub min_ago: &'static str,
    pub h_ago: &'static str,
    pub d_ago: &'static str,
    pub y_ago: &'static str,
    pub desktop_only: &'static str,
}

pub const EN: L10n = L10n {
    just_now: "just now",
    min_ago: "{} min ago",
    h_ago: "{} hours ago",
    d_ago: "{} days ago",
    y_ago: "{} years ago",
    desktop_only: "Documents can only be viewed on desktop",
};
pub const ZH_CN: L10n = L10n {
    just_now: "刚刚",
    min_ago: "{} 分钟前",
    h_ago: "{} 小时前",
    d_ago: "{} 天前",
    y_ago: "{} 年前",
    desktop_only: "文档只支持桌面端查看",
};
pub const DE: L10n = L10n {
    just_now: "gerade eben",
    min_ago: "vor {} Min",
    h_ago: "vor {} Std",
    d_ago: "vor {} T",
    y_ago: "vor {} J",
    desktop_only: "Dokumente können nur auf Desktop-Geräten angezeigt werden",
};
pub const ES: L10n = L10n {
    just_now: "ahora mismo",
    min_ago: "hace {} min",
    h_ago: "hace {} h",
    d_ago: "hace {} d",
    y_ago: "hace {} a",
    desktop_only: "Los documentos solo se pueden ver en escritorio",
};
pub const FR: L10n = L10n {
    just_now: "à l'instant",
    min_ago: "il y a {} min",
    h_ago: "il y a {} h",
    d_ago: "il y a {} j",
    y_ago: "il y a {} a",
    desktop_only: "Les documents ne peuvent être consultés que sur ordinateur de bureau",
};
pub const JA: L10n = L10n {
    just_now: "たった今",
    min_ago: "{} 分前",
    h_ago: "{} 時間前",
    d_ago: "{} 日前",
    y_ago: "{} 年前",
    desktop_only: "ドキュメントはデスクトップ版でのみ表示できます",
};
pub const RU: L10n = L10n {
    just_now: "только что",
    min_ago: "{} мин назад",
    h_ago: "{} ч назад",
    d_ago: "{} дн назад",
    y_ago: "{} г назад",
    desktop_only: "Документы можно просматривать только на настольной версии",
};
pub const AR: L10n = L10n {
    just_now: "الآن",
    min_ago: "قبل {} دقيقة",
    h_ago: "قبل {} ساعة",
    d_ago: "قبل {} يوم",
    y_ago: "قبل {} سنة",
    desktop_only: "يمكن عرض المستندات على سطح المكتب فقط",
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

/// The [`L10n`] catalog for the current app language (falls back to English
/// when no language has been set).
pub fn cur_l10n() -> &'static L10n {
    match LANG.get() {
        Some(cell) => {
            let g = cell.read().unwrap();
            table(g.as_str())
        }
        None => &EN,
    }
}

/// Translate the per-type inbound-notification body ("Text" / "Image" / "File"
/// / "Voice") into the app's current language. Uses the thread-safe language
/// cache set by [`set_lang`]. Falls back to English.
///
/// `kind` is one of `"text"`, `"image"`, `"file"`, `"voice"` (any other value
/// yields the generic "message" label).
pub fn translate_notify_body(kind: &str) -> String {
    let lang = get_lang();

    match lang.as_str() {
        "zh_CN" | "zh" => match kind {
            "text" => "文本消息",
            "image" => "图片",
            "file" => "文件",
            "voice" => "语音",
            _ => "消息",
        },
        "ja" => match kind {
            "text" => "テキストメッセージ",
            "image" => "画像",
            "file" => "ファイル",
            "voice" => "ボイス",
            _ => "メッセージ",
        },
        "de" => match kind {
            "text" => "Textnachricht",
            "image" => "Bild",
            "file" => "Datei",
            "voice" => "Sprachnachricht",
            _ => "Nachricht",
        },
        "fr" => match kind {
            "text" => "Message texte",
            "image" => "Image",
            "file" => "Fichier",
            "voice" => "Vocal",
            _ => "Message",
        },
        "ru" => match kind {
            "text" => "Текстовое сообщение",
            "image" => "Изображение",
            "file" => "Файл",
            "voice" => "Голосовое",
            _ => "Сообщение",
        },
        "es" => match kind {
            "text" => "Mensaje de texto",
            "image" => "Imagen",
            "file" => "Archivo",
            "voice" => "Voz",
            _ => "Mensaje",
        },
        "ar" => match kind {
            "text" => "رسالة نصية",
            "image" => "صورة",
            "file" => "ملف",
            "voice" => "صوت",
            _ => "رسالة",
        },
        _ => match kind {
            "text" => "Text message",
            "image" => "Image",
            "file" => "File",
            "voice" => "Voice",
            _ => "Message",
        },
    }
    .to_string()
}
