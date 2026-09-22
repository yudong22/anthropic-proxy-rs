use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Claude Code's user settings file.
pub fn settings_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".claude/settings.json"))
}

/// Model slots Claude Code reads from `env`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SlotUpdate {
    /// Slot name: sonnet | opus | haiku | model
    pub slot: String,
    /// Upstream model id, e.g. `deepseek-v4.1-flash`
    pub model: String,
    /// Optional human-friendly display name
    #[serde(default)]
    pub name: Option<String>,
}

/// Read the current env map (and top-level `model`) for display.
pub fn read_current() -> Result<Value> {
    let path = settings_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    if !path.exists() {
        return Ok(json!({ "exists": false, "env": {}, "model": null }));
    }
    let text = std::fs::read_to_string(&path)?;
    let doc: Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("invalid JSON: {}", e))?;

    let env = doc.get("env").cloned().unwrap_or_else(|| json!({}));
    let model = doc.get("model").cloned().unwrap_or(Value::Null);

    Ok(json!({
        "exists": true,
        "path": path.display().to_string(),
        "env": env,
        "model": model,
    }))
}

/// Apply model slots, preserving every unrelated key.
pub fn apply_slots(updates: &[SlotUpdate], base_url: &str) -> Result<Value> {
    let path = settings_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;

    let mut doc: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("invalid JSON: {}", e))?
    } else {
        json!({ "env": {} })
    };

    if !doc.is_object() {
        bail!("settings.json must be a JSON object");
    }

    let env = doc
        .as_object_mut()
        .and_then(|o| o.get_mut("env"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("settings.json has no `env` object"))?;

    for u in updates {
        match u.slot.as_str() {
            "model" => {
                env.insert("ANTHROPIC_MODEL".into(), json!(u.model));
            }
            "sonnet" | "opus" | "haiku" => {
                let key = format!("ANTHROPIC_DEFAULT_{}_MODEL", u.slot.to_uppercase());
                let name_key = format!("{}_NAME", key);
                env.insert(key, json!(u.model));
                env.insert(
                    name_key,
                    json!(u.name.clone().unwrap_or_else(|| u.model.clone())),
                );
            }
            other => bail!("unknown model slot: {}", other),
        }
    }

    env.insert("ANTHROPIC_BASE_URL".into(), json!(base_url));

    // Create a backup so a bad edit is recoverable.
    if path.exists() {
        let backup = path.with_extension("json.bak");
        let _ = std::fs::copy(&path, backup);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;

    Ok(json!({ "ok": true, "path": path.display().to_string() }))
}
