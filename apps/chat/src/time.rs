use chrono::{DateTime, Local, TimeZone};

fn local_datetime(ms: i64) -> Option<DateTime<Local>> {
    chrono::Local.timestamp_opt(ms / 1000, (ms % 1000) as u32 * 1_000_000).single()
}

pub fn time_label(ms: i64) -> String {
    if ms <= 0 {
        return "just now".into();
    }
    match local_datetime(ms) {
        Some(dt) => dt.format("%H:%M").to_string(),
        None => "just now".into(),
    }
}

pub fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let now = Local::now();
    let dt = match local_datetime(ms) {
        Some(dt) => dt,
        None => return String::new(),
    };
    let diff = (now - dt).num_milliseconds();
    let diff = diff.max(0);
    if diff < 60_000 {
        "just now".into()
    } else if diff < 3_600_000 {
        format!("{} min ago", diff / 60_000)
    } else if diff < 86_400_000 {
        format!("{} hours ago", diff / 3_600_000)
    } else if diff < 86_400_000 * 365 {
        format!("{} days ago", diff / 86_400_000)
    } else {
        format!("{} years ago", diff / 86_400_000 / 365)
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
