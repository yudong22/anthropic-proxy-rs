use anyhow::{bail, Result};
use std::path::PathBuf;

/// Manage a macOS LaunchAgent for "launch at login".
/// Label: com.anthropic-proxy.gui
const LABEL: &str = "com.anthropic-proxy.gui";

pub fn plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", LABEL))
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot resolve current exe: {}", e))
}

fn plist_content(exe: &str, workdir: &str, log_path: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>--gui-child</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{workdir}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LABEL,
        exe = exe,
        workdir = workdir,
        log = log_path
    )
}

pub fn install() -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("launch-at-login is only supported on macOS");
    }
    let exe = current_exe()?;
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Keep stdout/stderr in ~/.proxy-rs/logs so an app reinstall keeps history.
    let log_path = crate::settings::log_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/tmp/anthropic-proxy.log".to_string());
    std::fs::write(
        &path,
        plist_content(
            &exe.display().to_string(),
            &workdir.display().to_string(),
            &log_path,
        ),
    )?;
    // Load (idempotent)
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid()?, LABEL)])
        .output();
    let out = std::process::Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{}", uid()?),
            &path.display().to_string(),
        ])
        .output()?;
    if !out.status.success() {
        let legacy = std::process::Command::new("launchctl")
            .args(["load", &path.display().to_string()])
            .output()?;
        if !legacy.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    Ok(path)
}

pub fn uninstall() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("launch-at-login is only supported on macOS");
    }
    let path = plist_path();
    if path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{}", uid()?, LABEL)])
            .output();
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &path.display().to_string()])
            .output();
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn is_installed() -> bool {
    plist_path().exists()
}

fn uid() -> Result<u32> {
    let out = std::process::Command::new("id").arg("-u").output()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim()
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("cannot parse uid: {}", e))
}
