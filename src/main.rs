use std::env;
use std::io;
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use crossterm::cursor::Show as CursorShow;
use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, LeaveAlternateScreen};
use tracing::info;

use tide::app;
use tide::launcher;

fn init_logging() {
    use std::sync::Mutex;
    use tracing_subscriber::EnvFilter;

    let log_val = match env::var("TIDE_LOG") {
        Ok(value) => value,
        Err(_) => return,
    };

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/tide.log")
    {
        Ok(file) => file,
        Err(_) => return,
    };

    let filter = EnvFilter::try_new(&log_val).unwrap_or_else(|_| EnvFilter::new("debug"));

    tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(false)
        .init();
}

fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            CursorShow,
            LeaveAlternateScreen
        );
        original_hook(info);
    }));
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            CursorShow,
            LeaveAlternateScreen
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    launcher::launch_if_needed().await?;

    init_logging();

    let session_name = launcher::target_session_name();
    info!(session = %session_name, "starting tide");

    install_panic_hook();
    enable_raw_mode()?;
    let guard = TerminalGuard;

    let restart = app::run(session_name).await?;

    if restart {
        info!("restarting tide via exec");
        drop(guard);
        let exe = env::current_exe().context("failed to get current exe path")?;
        let args: Vec<String> = env::args().collect();
        let err = std::process::Command::new(&exe)
            .args(&args[1..])
            .env("TIDE_RESTARTED", "1")
            .exec();
        anyhow::bail!("exec failed: {err}");
    }

    info!("tide shut down");
    Ok(())
}
