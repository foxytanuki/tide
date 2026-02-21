use std::env;

use anyhow::Result;
use tokio::process::Command;

pub const TMUXIDE_SESSION_NAME: &str = "tmuxide";

/// If we're not already in the sidebar process, spawn sidebar in tmuxide session and exit.
/// Returns `Ok(())` if we should continue as the sidebar process.
pub async fn launch_if_needed() -> Result<()> {
    if env::var("TMUXIDE_SIDEBAR").is_ok() {
        return Ok(());
    }

    let inner_cmd = build_sidebar_inner_cmd();

    if env::var("TMUX").is_ok() {
        let current_session = detect_session_name().await;
        ensure_session_exists(TMUXIDE_SESSION_NAME)?;

        if current_session != TMUXIDE_SESSION_NAME {
            split_sidebar_in_session(TMUXIDE_SESSION_NAME, &inner_cmd, true)?;
            switch_client_to_session(TMUXIDE_SESSION_NAME)?;
            std::process::exit(0);
        }

        split_sidebar_in_session(TMUXIDE_SESSION_NAME, &inner_cmd, false)?;
        std::process::exit(0);
    } else {
        ensure_session_exists(TMUXIDE_SESSION_NAME)?;
        split_sidebar_in_session(TMUXIDE_SESSION_NAME, &inner_cmd, true)?;
        attach_to_session(TMUXIDE_SESSION_NAME)?;
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
        .unwrap_or_else(|_| "tmuxide".into())
        .display()
        .to_string();
    let args: Vec<String> = env::args().skip(1).collect();

    let mut inner_cmd = String::from("TMUXIDE_SIDEBAR=1");
    if let Ok(log) = env::var("TMUXIDE_LOG") {
        inner_cmd.push_str(&format!(" TMUXIDE_LOG={}", shell_quote(&log)));
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
    let status = std::process::Command::new("tmux")
        .args(["new-session", "-Ad", "-s", session])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to ensure tmux session '{}'", session);
    }
}

fn split_sidebar_in_session(session: &str, inner_cmd: &str, detached: bool) -> Result<()> {
    let mut cmd = std::process::Command::new("tmux");
    cmd.arg("split-window").arg("-t").arg(session);
    if detached {
        cmd.arg("-d");
    }
    let status = cmd
        .args(["-hb", "-l", "30", "--", "sh", "-c", inner_cmd])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to split sidebar in session '{}'", session);
    }
}

fn switch_client_to_session(session: &str) -> Result<()> {
    let status = std::process::Command::new("tmux")
        .args(["switch-client", "-t", session])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to switch client to session '{}'", session);
    }
}

fn attach_to_session(session: &str) -> Result<()> {
    let status = std::process::Command::new("tmux")
        .args(["attach-session", "-t", session])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("failed to attach to session '{}'", session);
    }
}
