//! Human-readable formatting for times, durations, sizes and rates.

/// Device monotonic time in nanoseconds (PROTOCOL.md §1).
pub type Ts = u64;

pub const NS_PER_MS: u64 = 1_000_000;
pub const NS_PER_SEC: u64 = 1_000_000_000;

/// Offset from the session origin as `mm:ss.mmm` (or `h:mm:ss.mmm` past an hour).
pub fn offset(ns: u64) -> String {
    let ms = ns / NS_PER_MS;
    let (h, rem) = (ms / 3_600_000, ms % 3_600_000);
    let (m, rem) = (rem / 60_000, rem % 60_000);
    let (s, ms) = (rem / 1000, rem % 1000);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{ms:03}") } else { format!("{m:02}:{s:02}.{ms:03}") }
}

/// Offset with only as much precision as a tick spacing needs (axis labels).
pub fn offset_label(ns: u64, tick_ns: u64) -> String {
    let full = offset(ns);
    if tick_ns >= NS_PER_SEC && ns.is_multiple_of(NS_PER_SEC) { full[..full.len() - 4].to_string() } else { full }
}

/// A duration like `305 ms`, `1.24 s`, `12.3 s`, `2 m 05 s`.
pub fn duration(ns: u64) -> String {
    let ms = ns as f64 / NS_PER_MS as f64;
    if ms < 1.0 {
        format!("{:.0} µs", ns as f64 / 1000.0)
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 10_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms < 60_000.0 {
        format!("{:.1} s", ms / 1000.0)
    } else {
        let s = (ms / 1000.0).round() as u64;
        format!("{} m {:02} s", s / 60, s % 60)
    }
}

/// Byte count: `225 B`, `3.4 KB`, `18 KB`, `3.1 MB` (1 KB = 1024 B).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 { format!("{v:.1} {}", UNITS[u]) } else { format!("{v:.0} {}", UNITS[u]) }
}

/// Rate in bytes per second: `0 B/s`, `12.4 KB/s`.
pub fn rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 0.5 {
        return "0 B/s".into();
    }
    format!("{}/s", bytes(bytes_per_sec.round() as u64))
}

/// A "nice" number at or above `v` from the 1-2-5 series (axis maxima).
pub fn nice_ceil(v: f64) -> f64 {
    if v <= 0.0 || !v.is_finite() {
        return 1.0;
    }
    let exp = v.log10().floor();
    let base = 10f64.powf(exp);
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if base * m >= v * 0.999_999 {
            return base * m;
        }
    }
    base * 10.0
}

/// A "nice" byte rate at or above `v`, on 1024-based steps so axis labels stay round (`512 KB/s`).
pub fn nice_rate_ceil(v: f64) -> f64 {
    if v <= 0.0 || !v.is_finite() {
        return 1024.0;
    }
    let mut unit = 1.0;
    while v >= unit * 1024.0 {
        unit *= 1024.0;
    }
    for m in [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0] {
        if unit * m >= v {
            return unit * m;
        }
    }
    unit * 1024.0
}

/// Wall-clock time of day in local time, `HH:MM:SS.mmm`, from Unix milliseconds.
pub fn wall_clock(unix_ms: i64) -> String {
    match jiff_like::local_hms(unix_ms) {
        Some((h, m, s, ms)) => format!("{h:02}:{m:02}:{s:02}.{ms:03}"),
        None => "--:--:--.---".into(),
    }
}

mod jiff_like {
    //! Minimal local-time conversion without a timezone database: the UI passes a fixed UTC
    //! offset (see [`set_local_offset_secs`]). Keeps `core` free of platform time zone code.
    use std::sync::atomic::{AtomicI64, Ordering};

    static OFFSET_SECS: AtomicI64 = AtomicI64::new(0);

    pub fn set(offset_secs: i64) {
        OFFSET_SECS.store(offset_secs, Ordering::Relaxed);
    }

    pub fn local_hms(unix_ms: i64) -> Option<(i64, i64, i64, i64)> {
        let local_ms = unix_ms.checked_add(OFFSET_SECS.load(Ordering::Relaxed) * 1000)?;
        let day_ms = local_ms.rem_euclid(86_400_000);
        Some((day_ms / 3_600_000, (day_ms / 60_000) % 60, (day_ms / 1000) % 60, day_ms % 1000))
    }
}

/// Set the UTC offset used by [`wall_clock`] (the UI resolves the local zone once at start-up).
pub fn set_local_offset_secs(offset_secs: i64) {
    jiff_like::set(offset_secs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(offset(7 * 60 * NS_PER_SEC + 10 * NS_PER_SEC + 5 * NS_PER_MS), "07:10.005");
        assert_eq!(offset(3_723 * NS_PER_SEC), "1:02:03.000");
        assert_eq!(offset_label(65 * NS_PER_SEC, 5 * NS_PER_SEC), "01:05");
        assert_eq!(offset_label(65 * NS_PER_SEC + 500 * NS_PER_MS, 500 * NS_PER_MS), "01:05.500");
        assert_eq!(duration(305 * NS_PER_MS), "305 ms");
        assert_eq!(duration(2_273 * NS_PER_MS), "2.27 s");
        assert_eq!(duration(125 * NS_PER_SEC), "2 m 05 s");
        assert_eq!(bytes(225), "225 B");
        assert_eq!(bytes(3482), "3.4 KB");
        assert_eq!(bytes(18 * 1024 + 100), "18 KB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(rate(12_697.6), "12 KB/s");
        assert_eq!(rate(0.1), "0 B/s");
    }

    #[test]
    fn nice_numbers() {
        assert_eq!(nice_ceil(3.7), 5.0);
        assert_eq!(nice_ceil(120.0), 200.0);
        assert_eq!(nice_rate_ceil(300.0 * 1024.0), 512.0 * 1024.0);
        assert_eq!(nice_rate_ceil(900.0), 1024.0);
        assert_eq!(nice_rate_ceil(1.5 * 1024.0 * 1024.0), 2.0 * 1024.0 * 1024.0);
    }

    #[test]
    fn wall_clock_uses_offset() {
        set_local_offset_secs(5 * 3600 + 1800);
        assert_eq!(wall_clock(0), "05:30:00.000");
        set_local_offset_secs(0);
        assert_eq!(wall_clock(1_790_658_651_123), "05:10:51.123");
    }
}
