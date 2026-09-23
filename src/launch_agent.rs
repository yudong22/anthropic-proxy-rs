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

fn plist_content(exe: &str, workdir: &str, process_log_path: &str) -> String {
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
        log = process_log_path
    )
}

/// Path for launchd's own stdout/stderr capture.
///
/// Deliberately **not** `proxy.log`: that file has exactly one writer, the
/// `LogBuffer` writer thread, which appends whole lines through a single
/// handle. launchd's stdout/stderr redirection is a second, independent writer
/// that knows nothing about that ordering, so pointing both at one file lets
/// the OS splice a write into the middle of a line the app is writing —
/// corrupting the very log lines used to diagnose request problems.
fn process_log_path() -> Option<PathBuf> {
    Some(crate::settings::log_dir()?.join("process.log"))
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
    // Keep the process capture in ~/.proxy-rs/logs so an app reinstall keeps
    // history. Separate from proxy.log on purpose — see [`process_log_path`].
    let log_path = process_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/tmp/anthropic-proxy-process.log".to_string());
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

/// Unload the job but keep the plist, so the next login still auto-starts.
///
/// This is what a user-initiated quit needs. The plist sets `KeepAlive`, so
/// launchd restarts the process the instant it exits — meaning a plain
/// `app.exit(0)` looks like a crash to launchd and the app comes straight back,
/// which is indistinguishable from "the app cannot be closed". Booting the job
/// out first removes the reason to relaunch, while leaving the plist on disk so
/// launch-at-login survives for next time.
///
/// Safe to call when the job is not loaded: `bootout` on an unknown job reports
/// a non-zero status that is deliberately ignored.
pub fn suspend() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    if !plist_path().exists() {
        return Ok(());
    }
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid()?, LABEL)])
        .output();
    Ok(())
}

/// Whether this process was started by launchd rather than by a user.
///
/// The plist passes `--gui-child` as its second argument. It exists because a
/// launchd-managed instance and a user-launched one need different shutdown
/// behaviour, and without the flag there is no way to tell them apart.
pub fn running_as_launchd_child(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|a| a == "--gui-child")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gui_child_flag_is_recognised() {
        // The plist's exact argv, so the flag stays in sync with the plist.
        assert!(running_as_launchd_child(args(&[
            "/Applications/Anthropic Proxy.app/Contents/MacOS/anthropic-proxy-gui",
            "--gui-child"
        ])));
    }

    #[test]
    fn a_user_launch_is_not_mistaken_for_a_launchd_child() {
        assert!(!running_as_launchd_child(args(&[
            "/Applications/Anthropic Proxy.app/Contents/MacOS/anthropic-proxy-gui"
        ])));
        // macOS passes `-psn_…` when launching from Finder; it must not match.
        assert!(!running_as_launchd_child(args(&[
            "/Applications/Anthropic Proxy.app/Contents/MacOS/anthropic-proxy-gui",
            "-psn_0_12345"
        ])));
    }

    #[test]
    fn plist_declares_keepalive_and_the_child_flag() {
        // KeepAlive is why a plain exit is not enough to quit; if this ever
        // changes, `quit_app`'s suspend step must be revisited.
        let content = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        assert!(content.contains("<key>KeepAlive</key>"));
        assert!(content.contains("<string>--gui-child</string>"));
        assert!(content.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn plist_captures_process_output_outside_the_apps_own_log() {
        // launchd's stdout/stderr redirection does not take `LogBuffer`'s write
        // mutex, so sharing `proxy.log` lets the OS splice a write into the
        // middle of a line the app is writing. The two files must stay distinct.
        let content = plist_content("/tmp/app", "/tmp", "/tmp/process.log");
        assert!(content.contains("/tmp/process.log"));
        assert!(
            !content.contains("proxy.log"),
            "the process capture must not be the app's own log file"
        );

        let capture = process_log_path().expect("log dir resolves");
        let app_log = crate::settings::log_file_path().expect("log path resolves");
        assert_ne!(capture, app_log);
    }
}
