//! Small shared helpers used across the proxy, stats and credits modules.

use axum::http::HeaderMap;

/// Truncate `text` to at most `max` characters, appending an ellipsis when cut.
///
/// Character-based (never byte-based) so it cannot split a multi-byte UTF-8
/// sequence and panic, which is why every caller funnels through here instead
/// of slicing strings directly.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut end = 0;
    for (i, ch) in text.char_indices() {
        if i > max {
            break;
        }
        end = i + ch.len_utf8();
    }
    format!("{}…", &text[..end])
}

/// Render a header map as a single `k: v | k: v` log line.
pub fn format_headers(headers: &HeaderMap) -> String {
    headers
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("<non-utf8>")))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Convert days since the Unix epoch to a `(year, month, day)` civil date.
///
/// Howard Hinnant's `civil_from_days` algorithm, shared by the log timestamp,
/// daily-stats and credits date rendering.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format epoch seconds as a local `YYYY-MM-DD HH:MM:SS` string (UTC offset
/// applied by the caller). Avoids pulling in `chrono`.
pub fn format_epoch_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

/// Local UTC offset in seconds, resolved once per call from the system zone.
///
/// Resolution order:
/// 1. `PROXY_TZ_OFFSET_HOURS` — explicit override, e.g. `"8"` for UTC+8.
/// 2. The system timezone, via `localtime_r`. This is what makes timestamps
///    follow the machine's zone instead of being hard-coded to +8; the result
///    includes DST, so it is re-read rather than assumed constant.
/// 3. UTC (0) if the system lookup somehow fails.
pub fn local_utc_offset_secs() -> i64 {
    if let Ok(v) = std::env::var("PROXY_TZ_OFFSET_HOURS") {
        if let Ok(h) = v.trim().parse::<i64>() {
            return h * 3600;
        }
    }
    system_utc_offset_secs().unwrap_or(0)
}

/// Ask libc for the current zone's UTC offset (e.g. 28800 for Asia/Shanghai).
///
/// `localtime_r` + `tm_gmtoff` is the direct answer on Unix, but Windows'
/// `libc::tm` has no `tm_gmtoff` field, so that path is `cfg`-gated and
/// Windows falls back to deriving the offset from the broken-down local time.
#[cfg(unix)]
fn system_utc_offset_secs() -> Option<i64> {
    let secs = now_secs()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a valid `libc::tm` out-param; `localtime_r` fills it on
    // success and returns null (without touching it) on failure.
    let ok = unsafe { libc::localtime_r(&(secs as libc::time_t), &mut tm) };
    if ok.is_null() {
        return None;
    }
    Some(tm.tm_gmtoff as i64)
}

/// Windows has no `tm_gmtoff`, so derive the offset by asking for both the
/// local and UTC broken-down time and measuring the difference.
///
/// The day fields matter, not just the time-of-day: UTC+13 and UTC-11 both show
/// a 1-hour-vs-12-hour split, and only the date tells them apart.
#[cfg(not(unix))]
fn system_utc_offset_secs() -> Option<i64> {
    let secs = now_secs()?;
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    let mut utc: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both are valid `libc::tm` out-params, as above.
    if unsafe { libc::localtime_r(&(secs as libc::time_t), &mut local) }.is_null() {
        return None;
    }
    if unsafe { libc::gmtime_r(&(secs as libc::time_t), &mut utc) }.is_null() {
        return None;
    }

    let local_days = days_from_civil(
        local.tm_year as i64 + 1900,
        local.tm_mon as i64 + 1,
        local.tm_mday as i64,
    );
    let utc_days = days_from_civil(
        utc.tm_year as i64 + 1900,
        utc.tm_mon as i64 + 1,
        utc.tm_mday as i64,
    );
    let local_sod = local.tm_hour as i64 * 3600 + local.tm_min as i64 * 60 + local.tm_sec as i64;
    let utc_sod = utc.tm_hour as i64 * 3600 + utc.tm_min as i64 * 60 + utc.tm_sec as i64;

    Some((local_days - utc_days) * 86_400 + (local_sod - utc_sod))
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
#[cfg(not(unix))]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_secs() -> Option<i64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if secs > i64::MAX as u64 {
        return None;
    }
    Some(secs as i64)
}

/// Format the current instant as a local `YYYY-MM-DD HH:MM:SS.mmm` string.
///
/// This is the single timestamp source for log lines and stats rows, so both
/// agree and both honour the machine's timezone.
pub fn local_datetime_millis() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let local = (now.as_secs() as i64 + local_utc_offset_secs()).max(0) as u64;
    format!("{}.{:03}", format_epoch_secs(local), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_and_marks_long_text() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("  hi  ", 10), "hi");
        assert!(truncate("hello world", 5).ends_with('…'));
    }

    #[test]
    fn truncate_does_not_split_multibyte_characters() {
        // Each char is 3 bytes; a byte-indexed cut here would panic.
        let text = "中文中文中文";
        let out = truncate(text, 4);
        assert!(out.ends_with('…'));
        assert!(out.starts_with('中'));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn format_epoch_secs_renders_utc_components() {
        assert_eq!(format_epoch_secs(0), "1970-01-01 00:00:00");
        assert_eq!(format_epoch_secs(86_400 + 3661), "1970-01-02 01:01:01");
    }
}
