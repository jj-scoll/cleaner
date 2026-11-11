//! Human-readable sizes and times.

/// `t` (Unix seconds) in `tz` as "%Y-%m-%d %H:%M"; the raw number if out of range.
pub fn local_time(t: i64, tz: &jiff::tz::TimeZone) -> String {
    match jiff::Timestamp::from_second(t) {
        Ok(ts) => ts.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M").to_string(),
        Err(_) => t.to_string(),
    }
}

/// Binary units with one decimal above bytes: "0 B", "512 B", "1.5 KiB", "71.4 GiB".
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    // 1023.95 would print as "1024.0", so step up a unit instead.
    while v >= 1023.95 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}
