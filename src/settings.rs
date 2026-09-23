use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::RwLock;

/// One log entry in the in-memory ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub ts: String,
    pub level: String,
    pub message: String,
}

/// Size-capped in-memory log history for GUI / dashboard.
pub struct LogBuffer {
    entries: RwLock<Vec<LogEntry>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(LogBuffer::load_persisted(capacity)),
            capacity,
        }
    }

    pub async fn push(&self, level: &str, message: String) {
        let entry = LogEntry {
            ts: chrono_now(),
            level: level.to_string(),
            message,
        };
        {
            let mut entries = self.entries.write().await;
            if entries.len() >= self.capacity {
                let overflow = entries.len() + 1 - self.capacity;
                entries.drain(..overflow);
            }
            entries.push(entry.clone());
        }
        // Mirror to ~/.proxy-rs/logs/proxy.log so history survives restarts.
        append_log_line(&format!("{} [{}] {}", entry.ts, entry.level, entry.message));
    }

    /// Recent lines from the on-disk log (used to seed the buffer at startup).
    pub fn load_persisted(capacity: usize) -> Vec<LogEntry> {
        let Some(path) = log_file_path() else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(capacity);
        lines[start..]
            .iter()
            .filter_map(|line| {
                // Format: "YYYY-MM-DD HH:MM:SS.mmm [LEVEL] message"
                let (ts, rest) = line.split_once(" [")?;
                let (level, message) = rest.split_once("] ")?;
                Some(LogEntry {
                    ts: ts.to_string(),
                    level: level.to_string(),
                    message: message.to_string(),
                })
            })
            .collect()
    }

    pub async fn snapshot(&self) -> Vec<LogEntry> {
        self.entries.read().await.clone()
    }

    pub async fn clear(&self) {
        self.entries.write().await.clear();
        if let Some(path) = log_file_path() {
            let _ = std::fs::write(&path, "");
        }
    }
}

/// Local wall-clock timestamp for a log line: `YYYY-MM-DD HH:MM:SS.mmm`.
/// Local here means the machine's timezone (`PROXY_TZ_OFFSET_HOURS` overrides).
fn chrono_now() -> String {
    crate::util::local_datetime_millis()
}

/// Default listen port; if taken at startup, the proxy falls back to +1.
pub const DEFAULT_PORT: u16 = 3456;

/// Persisted proxy and GUI settings in `~/.proxy-rs/gui-settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiSettings {
    pub provider_id: String,
    /// Custom endpoint override (empty = use preset default)
    pub custom_url: String,
    pub api_key: String,
    pub port: u16,
    pub bind: String,
    #[serde(default)]
    pub reasoning_model: String,
    #[serde(default)]
    pub completion_model: String,
    #[serde(default)]
    pub model_map: String,
    #[serde(default)]
    pub launch_at_login: bool,
    /// Optional semicolon-separated list of fixed-template phrases to neutralize
    /// in the outbound request (WorkBuddy/CodeBuddy content-filter fingerprints).
    /// Forwarded to the Rust sanitize pass via `Config::sanitize_fingerprints`;
    /// exposed in the GUI so it can be edited without restarting via env vars.
    #[serde(default)]
    pub sanitize_terms: String,
    /// Whether the upstream only serves streaming bodies.
    ///
    /// `None` follows the provider preset's own `force_stream` flag (the
    /// default, and what a newly added provider gets from `builtin_presets`);
    /// `Some(true/false)` is the GUI switch, which also covers a custom URL
    /// whose preset we do not know. When enabled, every request goes upstream
    /// as a stream and a non-streaming client still gets one JSON body.
    #[serde(default)]
    pub force_stream: Option<bool>,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            provider_id: "workbuddy-cn".to_string(),
            custom_url: String::new(),
            api_key: String::new(),
            port: 3456,
            bind: "127.0.0.1".to_string(),
            reasoning_model: String::new(),
            completion_model: String::new(),
            model_map: String::new(),
            launch_at_login: false,
            sanitize_terms: String::new(),
            force_stream: None,
        }
    }
}

/// Canonical data directory: `~/.proxy-rs`.
pub fn data_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".proxy-rs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Stable settings path: `~/.proxy-rs/gui-settings.json`.
pub fn settings_path() -> PathBuf {
    if let Some(dir) = data_dir() {
        return dir.join("gui-settings.json");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("gui-settings.json")
}

/// `.env` config path: `~/.proxy-rs/.env`.
pub fn dotenv_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(".env"))
}

/// Log directory: `~/.proxy-rs/logs`.
pub fn log_dir() -> Option<PathBuf> {
    let dir = data_dir()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Serializes log writes so concurrent request handlers cannot interleave
/// half-written lines.
///
/// `writeln!` issues one `write` syscall per format fragment, so without an
/// exclusive handle two tasks interleave half-written lines — exactly the torn
/// output this guards against. The lock is held across the whole line (body +
/// newline) so no other writer can split it.
static LOG_FILE: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

/// Test hook: forces log mirroring to a temp file instead of the user's
/// `~/.proxy-rs/logs/proxy.log`. Re-settable so two tests can each redirect it.
static LOG_FILE_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn append_log_line(line: &str) {
    let Some(path) = log_file_path() else {
        return;
    };
    let mut guard = match LOG_FILE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        *guard = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
    }
    if let Some(file) = guard.as_mut() {
        use std::io::Write;
        // Body and newline are written while holding the lock, so no other
        // writer can land between them and tear the line.
        let _ = file.write_all(line.as_bytes());
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }
}

/// Rolling log file: `~/.proxy-rs/logs/proxy.log`.
pub fn log_file_path() -> Option<PathBuf> {
    if let Ok(guard) = LOG_FILE_OVERRIDE.lock() {
        if let Some(p) = guard.as_ref() {
            return Some(p.clone());
        }
    }
    Some(log_dir()?.join("proxy.log"))
}

/// Point log mirroring at `path` for the rest of the process (tests only).
#[cfg(test)]
fn set_log_file_override(path: PathBuf) {
    // Drop any handle pointing at the previous location.
    if let Ok(mut g) = LOG_FILE.lock() {
        *g = None;
    }
    if let Ok(mut g) = LOG_FILE_OVERRIDE.lock() {
        *g = Some(path);
    }
}

impl GuiSettings {
    pub fn load() -> Self {
        let path = settings_path();
        let mut settings = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => {
                // Migrate a legacy settings file from an old working directory.
                let legacy = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("gui-settings.json");
                match std::fs::read_to_string(&legacy) {
                    Ok(text) => {
                        let s: GuiSettings = serde_json::from_str(&text).unwrap_or_default();
                        let _ = std::fs::write(&path, serde_json::to_string_pretty(&s).unwrap());
                        s
                    }
                    Err(_) => Self::default(),
                }
            }
        };
        settings.port = if settings.port == 0 {
            DEFAULT_PORT
        } else {
            settings.port
        };
        settings
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Effective upstream chat completions URL.
    pub fn chat_url(&self, presets: &[crate::providers::ProviderPreset]) -> String {
        if !self.custom_url.trim().is_empty() {
            return normalize_chat_url(self.custom_url.trim());
        }
        if let Some(p) = presets.iter().find(|p| p.id == self.provider_id) {
            return p.chat_completions_url.clone();
        }
        // Unknown provider falls back to workbuddy-cn
        crate::providers::builtin_presets()[0]
            .chat_completions_url
            .clone()
    }

    /// Whether upstream requests must be sent as a stream.
    ///
    /// The GUI switch wins when set; otherwise the provider preset decides.
    /// A custom URL keeps plain semantics unless the user flips the switch or
    /// the runtime `11101` detector discovers it.
    pub fn force_stream(&self, presets: &[crate::providers::ProviderPreset]) -> bool {
        self.force_stream
            .unwrap_or_else(|| crate::providers::preset_force_stream(presets, &self.provider_id))
    }

    pub fn models_preset(&self) -> crate::providers::ProviderPreset {
        let presets = crate::providers::builtin_presets();
        presets
            .iter()
            .find(|p| p.id == self.provider_id)
            .cloned()
            .unwrap_or_else(|| presets[0].clone())
    }
}

/// Accepts base URL, versioned base, or full endpoint; returns full chat URL.
pub fn normalize_chat_url(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        return trimmed.to_string();
    }
    // Versioned base like .../v2
    let last = trimmed.rsplit('/').next().unwrap_or("");
    if last.len() > 1
        && (last.starts_with('v') || last.starts_with('V'))
        && last[1..].chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}/chat/completions", trimmed);
    }
    format!("{}/v1/chat/completions", trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminator for the long bodies in the concurrency test, so a line that
    /// got cut short is detectable rather than looking merely shorter.
    const END_MARKER: &str = " |end|";

    /// The log destination is process-global, so tests that assert on the file
    /// must not run concurrently with each other.
    static LOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Redirect log mirroring to a temp file so tests never touch (or fill)
    /// the user's real `~/.proxy-rs/logs/proxy.log`.
    ///
    /// Returns the temp path plus a guard held for the duration of the test.
    fn use_temp_log_file(name: &str) -> (std::path::PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = LOG_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("proxy-rs-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        set_log_file_override(path.clone());
        (path, guard)
    }

    #[test]
    fn normalize_full_url() {
        assert_eq!(
            normalize_chat_url("https://copilot.tencent.com/v2/chat/completions"),
            "https://copilot.tencent.com/v2/chat/completions"
        );
    }

    #[test]
    fn normalize_versioned_base() {
        assert_eq!(
            normalize_chat_url("https://gateway.example.com/v2/"),
            "https://gateway.example.com/v2/chat/completions"
        );
    }

    #[test]
    fn normalize_plain_base() {
        assert_eq!(
            normalize_chat_url("https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn concurrent_pushes_never_interleave_lines() {
        let (log_path, _guard) = use_temp_log_file("concurrent.log");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let expected = 40 * 50;
        rt.block_on(async {
            let buf = LogBuffer::new(expected + 500);
            let mut tasks = Vec::new();
            let buf = std::sync::Arc::new(buf);
            for i in 0..40 {
                let buf = buf.clone();
                tasks.push(tokio::spawn(async move {
                    for j in 0..50 {
                        // A long body makes a torn write obvious, and the
                        // trailing marker makes a truncated one obvious.
                        let body = format!("{}{}", "x".repeat(300), END_MARKER);
                        buf.push("INFO", format!("task {i} line {j} {body}")).await;
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }

            // The buffer is seeded from the log file, which other tests share,
            // so only this test's own lines are asserted on.
            let mine: Vec<_> = buf
                .snapshot()
                .await
                .into_iter()
                .filter(|e| e.message.starts_with("task "))
                .collect();
            assert_eq!(mine.len(), expected);
            for entry in &mine {
                let msg = &entry.message;
                assert!(msg.ends_with(END_MARKER), "truncated line: {msg:?}");
                assert!(msg.len() > 300, "shortened line: {msg:?}");
            }
        });

        // The real regression was on disk: every written line must carry
        // exactly one "[INFO] " marker and end with this test's body. A torn
        // line splices two writes together and therefore shows two markers.
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let mine: Vec<&str> = text.lines().filter(|l| l.contains("task ")).collect();
        assert_eq!(mine.len(), expected, "expected every push to reach disk");
        for line in mine {
            assert_eq!(
                line.matches(" [INFO] ").count(),
                1,
                "torn log line: {line:?}"
            );
            assert!(line.ends_with(END_MARKER), "truncated log line: {line:?}");
        }
    }

    #[test]
    fn log_buffer_trims_to_capacity() {
        let (_path, _guard) = use_temp_log_file("capacity.log");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let buf = LogBuffer::new(3);
            for i in 0..10 {
                buf.push("INFO", format!("line {}", i)).await;
            }
            let snap = buf.snapshot().await;
            assert_eq!(snap.len(), 3);
            assert_eq!(snap[0].message, "line 7");
            assert_eq!(snap[2].message, "line 9");
        });
    }

    #[test]
    fn unset_force_stream_follows_the_provider_preset() {
        let presets = crate::providers::builtin_presets();
        let mut s = GuiSettings::default();
        assert_eq!(s.force_stream, None, "default must follow the preset");

        s.provider_id = "workbuddy-cn".to_string();
        assert!(s.force_stream(&presets));

        s.provider_id = "openai".to_string();
        assert!(!s.force_stream(&presets));

        // A provider with no preset (custom URL) keeps plain semantics.
        s.provider_id = "custom-gateway".to_string();
        assert!(!s.force_stream(&presets));
    }

    #[test]
    fn gui_switch_overrides_the_preset_in_both_directions() {
        let presets = crate::providers::builtin_presets();

        // Turn streaming ON for a provider that serves plain bodies.
        let mut s = GuiSettings {
            provider_id: "openai".to_string(),
            force_stream: Some(true),
            ..Default::default()
        };
        assert!(s.force_stream(&presets));

        // ...and OFF for one whose preset forces it.
        s.provider_id = "workbuddy-cn".to_string();
        s.force_stream = Some(false);
        assert!(!s.force_stream(&presets));
    }

    #[test]
    fn force_stream_survives_a_settings_round_trip() {
        let s = GuiSettings {
            force_stream: Some(true),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: GuiSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.force_stream, Some(true));

        // Older settings files have no such key: they must load as "follow the
        // preset" rather than failing to deserialize.
        let legacy: GuiSettings = serde_json::from_str(
            r#"{"provider_id":"workbuddy-cn","custom_url":"","api_key":"","port":3456,"bind":"127.0.0.1"}"#,
        )
        .unwrap();
        assert_eq!(legacy.force_stream, None);
        assert!(legacy.force_stream(&crate::providers::builtin_presets()));
    }
}
