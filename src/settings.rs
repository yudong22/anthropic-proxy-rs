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
        if let Some(path) = log_file_path() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                use std::io::Write;
                let _ = writeln!(f, "{} [{}] {}", entry.ts, entry.level, entry.message);
            }
        }
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

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:03}",
        crate::util::format_epoch_secs(now.as_secs()),
        now.subsec_millis()
    )
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

/// Rolling log file: `~/.proxy-rs/logs/proxy.log`.
pub fn log_file_path() -> Option<PathBuf> {
    Some(log_dir()?.join("proxy.log"))
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
    fn log_buffer_trims_to_capacity() {
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
}
