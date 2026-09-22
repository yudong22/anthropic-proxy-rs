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
