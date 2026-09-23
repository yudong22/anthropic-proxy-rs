//! Small shared helpers used across the proxy, stats and credits modules.

use axum::{
    body::{to_bytes, Body},
    http::{HeaderMap, Request},
};

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
///
/// Credential-bearing headers are redacted first. These lines are written to
/// `~/.proxy-rs/logs/proxy.log`, which is long-lived and routinely copied into
/// bug reports, so an upstream API key must never land there in the clear. The
/// leading characters are kept so a key can still be told apart from another
/// while debugging.
pub fn format_headers(headers: &HeaderMap) -> String {
    headers
        .iter()
        .map(|(k, v)| {
            let value = v.to_str().unwrap_or("<non-utf8>");
            format!("{}: {}", k, redact_header_value(k.as_str(), value))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Header names whose value is a credential and must not be logged verbatim.
///
/// Matched exactly (case-insensitively) or by the `contains` rule below — see
/// [`is_sensitive_header`]. Providers do not agree on a single spelling, so the
/// common vendor variants are listed rather than relying on one canonical name.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "cookie",
    "set-cookie",
    "x-goog-api-key",
    "openai-api-key",
    "anthropic-api-key",
    "x-anthropic-api-key",
    "x-api-token",
];

/// Substrings that mark a header as credential-bearing even when the exact name
/// is not listed (`x-some-vendor-api-key`, `x-foo-token`, …).
const SENSITIVE_NAME_FRAGMENTS: &[&str] = &["api-key", "apikey", "auth-token", "access-token"];

fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if SENSITIVE_HEADERS.contains(&name.as_str()) {
        return true;
    }
    if name.ends_with("-token") || name.ends_with("-secret") {
        return true;
    }
    SENSITIVE_NAME_FRAGMENTS.iter().any(|f| name.contains(f))
}

/// Mask a credential, keeping a short prefix for identification.
///
/// `Bearer ck_abc…` keeps its scheme so the line still reads naturally; a bare
/// token keeps a very short prefix. The prefix is deliberately small and the
/// value is masked entirely when that prefix would be a meaningful fraction of
/// the secret — these lines end up in `proxy.log`, which is routinely attached
/// to bug reports.
fn redact_header_value(name: &str, value: &str) -> String {
    if !is_sensitive_header(name) {
        return value.to_string();
    }

    // Any `<scheme> <secret>` form: `Bearer …`, `Basic …`, `ApiKey …`, `SSWS …`.
    // Only a leading token followed by whitespace is treated as a scheme; a
    // bare secret containing spaces keeps its whitespace inside the mask.
    let (scheme, secret) = match value.split_once(' ') {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && rest.contains(|c: char| !c.is_whitespace()) =>
        {
            (Some(scheme), rest.trim())
        }
        _ => (None, value.trim()),
    };

    // Below this length a visible prefix would be a meaningful fraction of the
    // secret, so show nothing at all.
    const KEEP: usize = 4;
    let masked = if secret.chars().count() > KEEP + 8 {
        let prefix: String = secret.chars().take(KEEP).collect();
        format!("{prefix}…")
    } else {
        "…".to_string()
    };

    match scheme {
        Some(scheme) => format!("{scheme} {masked}"),
        None => masked,
    }
}

/// Parse a JSON body from the exact bytes the client sent.
///
/// Used at the top of a handler, before `Json<T>` consumes the body: `T` is the
/// *translation* model, which deliberately drops fields this proxy does not
/// forward, so the typed request cannot be the source of truth for something
/// the client told us (e.g. Claude Code's `metadata.user_id`). Reading the raw
/// bytes keeps that information without widening a translation type for what is
/// purely a logging concern.
///
/// Returns `None` on any problem — a non-JSON body, a body that will not buffer
/// — so a caller can treat a logging aid as strictly optional. The extracted
/// [`Request`] is handed back for `Json` to re-consume.
pub async fn peek_json_body(req: Request<Body>) -> (Option<serde_json::Value>, Request<Body>) {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = to_bytes(body, usize::MAX).await else {
        return (None, Request::from_parts(parts, Body::empty()));
    };
    let parsed = serde_json::from_slice(&bytes).ok();
    (parsed, Request::from_parts(parts, Body::from(bytes)))
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
    fn format_headers_redacts_credentials() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static(
                "Bearer ck_fm3j4t8apekg.AtU_2TMOY8pdXrOmXHTJPkm-hSbuLhroRjABd8flTgQ",
            ),
        );
        headers.insert(
            "x-api-key",
            HeaderValue::from_static("sk-ant-secret-value-here"),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let line = format_headers(&headers);

        assert!(
            !line.contains("AtU_2TMOY8pdXrOmXHTJPkm"),
            "key leaked: {line}"
        );
        assert!(!line.contains("secret-value-here"), "key leaked: {line}");
        assert!(
            line.contains("Bearer ck_f…"),
            "scheme+short prefix kept: {line}"
        );
        assert!(
            line.contains("content-type: application/json"),
            "others intact"
        );
    }

    #[test]
    fn format_headers_redacts_non_bearer_schemes_and_vendor_names() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-goog-api-key",
            HeaderValue::from_static("AIzaSyD-very-secret-google-key"),
        );
        headers.insert(
            "x-vendor-access-token",
            HeaderValue::from_static("vendor-token-value-123456"),
        );
        headers.insert(
            "authorization",
            HeaderValue::from_static("Basic dXNlcjpwYXNzd29yZA=="),
        );

        let line = format_headers(&headers);

        for secret in [
            "SyD-very-secret-google-key",
            "token-value-123456",
            "dXNlcjpwYXNzd29yZA",
        ] {
            assert!(!line.contains(secret), "leaked {secret}: {line}");
        }
        assert!(line.contains("Basic "), "scheme kept: {line}");
    }

    #[test]
    fn short_secrets_are_masked_entirely() {
        use axum::http::HeaderValue;
        // 12 chars: a visible prefix would be a meaningful fraction of it.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("shortsecret1"));
        let line = format_headers(&headers);
        assert!(!line.contains("short"), "leaked: {line}");
        assert!(!line.contains("secret1"), "leaked: {line}");
    }

    #[test]
    fn redaction_keeps_nothing_when_the_value_is_too_short() {
        // A short secret would be given away by any visible prefix.
        assert_eq!(redact_header_value("authorization", "abc"), "…");
    }

    #[test]
    fn redaction_leaves_non_credential_headers_alone() {
        assert_eq!(
            redact_header_value("user-agent", "deepseek-harness/0.1.6-alpha.2"),
            "deepseek-harness/0.1.6-alpha.2"
        );
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
