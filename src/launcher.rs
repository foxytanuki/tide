use std::env;

use anyhow::Result;
use tokio::process::Command;

pub const DEFAULT_SESSION_NAME: &str = "tide";

/// Determine the target session name from CLI arg, falling back to default.
pub fn target_session_name() -> String {
    match env::args().nth(1) {
        Some(s) if !s.trim().is_empty() => s,
        _ => DEFAULT_SESSION_NAME.to_string(),
    }
}

fn exact_session_window_target(session: &str) -> String {
    format!("={session}:")
}

fn exact_session_target(session: &str) -> String {
    format!("={session}")
}

/// If we're not already in the sidebar process, spawn sidebar in tide session and exit.
/// Returns `Ok(())` if we should continue as the sidebar process.
pub async fn launch_if_needed() -> Result<()> {
    if env::var("TIDE_SIDEBAR").ok().as_deref() == Some("1") {
        return Ok(());
    }

    let session = target_session_name();
    let inner_cmd = build_sidebar_inner_cmd();

    if env::var("TMUX").is_ok() {
        let current_session = detect_session_name().await;
        ensure_session_exists(&session)?;

        if current_session != session {
            split_sidebar_in_session(&session, &inner_cmd, true)?;
            switch_client_to_session(&session)?;
            std::process::exit(0);
        }

        split_sidebar_in_session(&session, &inner_cmd, false)?;
        std::process::exit(0);
    } else {
        ensure_session_exists(&session)?;
        split_sidebar_in_session(&session, &inner_cmd, true)?;
        attach_to_session(&session)?;
        std::process::exit(0);
    }
}

pub async fn detect_session_name() -> String {
    if env::var("TMUX").is_err() {
        return "main".to_string();
    }

    match Command::new("tmux")
        .args(["display-message", "-p", "#S"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                "main".to_string()
            } else {
                s
            }
        }
        _ => "main".to_string(),
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn build_sidebar_inner_cmd() -> String {
    let exe = env::current_exe()
        .unwrap_or_else(|_| "tide".into())
        .display()
        .to_string();
    let args: Vec<String> = env::args().skip(1).collect();

    let mut inner_cmd = String::from("TIDE_SIDEBAR=1");
    if let Ok(log) = env::var("TIDE_LOG") {
        inner_cmd.push_str(&format!(" TIDE_LOG={}", shell_quote(&log)));
    }
    inner_cmd.push(' ');
    inner_cmd.push_str(&shell_quote(&exe));
    for arg in &args {
        inner_cmd.push(' ');
        inner_cmd.push_str(&shell_quote(arg));
    }
    inner_cmd
}

fn ensure_session_exists(session: &str) -> Result<()> {
    let already = std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if already {
        return Ok(());
    }
    let initial_window = "general:tab1";
    let status = std::process::Command::new("tmux")
        .args(["new-session", "-d", "-s", session, "-n", initial_window])
        .env_remove("TMUX")
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to create tmux session '{}'", session);
    }
    let session_target = exact_session_window_target(session);

    // Disable automatic window renaming for the initial window
    let _ = std::process::Command::new("tmux")
        .args([
            "set-window-option",
            "-t",
            &session_target,
            "automatic-rename",
            "off",
        ])
        .status();
    let _ = std::process::Command::new("tmux")
        .args([
            "set-window-option",
            "-t",
            &session_target,
            "allow-rename",
            "off",
        ])
        .status();
    Ok(())
}

fn split_sidebar_in_session(session: &str, inner_cmd: &str, detached: bool) -> Result<()> {
    let session_target = exact_session_window_target(session);
    let mut cmd = std::process::Command::new("tmux");
    cmd.arg("split-window").arg("-t").arg(&session_target);
    if detached {
        cmd.arg("-d");
    }
    let status = cmd
        .args(["-fhb", "-l", "30", "--", "sh", "-c", inner_cmd])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to split sidebar in session '{}'", session);
    }
}

fn switch_client_to_session(session: &str) -> Result<()> {
    let session_target = exact_session_target(session);
    let status = std::process::Command::new("tmux")
        .args(["switch-client", "-t", &session_target])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to switch client to session '{}'", session);
    }
}

fn attach_to_session(session: &str) -> Result<()> {
    let session_target = exact_session_target(session);
    let status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", &session_target])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to attach to session '{}'", session);
    }
}
