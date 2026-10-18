pub fn time_label(ms: i64) -> String {
    if ms <= 0 {
        return "just now".into();
    }
    let secs = (ms / 1000) as i64;
    let day_start = (secs / 86_400) * 86_400;
    let h = ((secs - day_start) / 3600) % 24;
    let m = ((secs - day_start) % 3600) / 60;
    format!("{h:02}:{m:02}")
}

pub fn fmt_time(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff = now.saturating_sub(ms);
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
    let t = if ms > 10_000_000_000 { ms / 1000 } else { ms };
    let (y, m, d) = civil_date((t / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn civil_date(z: i64) -> (i64, i64, i64) {
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
