mod cmd;
mod execute;
mod launcher;
mod model;
mod msg;
mod tmux;
mod tree;
mod update;
mod view;

use std::env;
use std::io;
use std::os::unix::process::CommandExt;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::cmd::Cmd;
use crate::execute::execute_commands;
use crate::model::Model;
use crate::msg::Msg;
use crate::tmux::{TmuxControl, TmuxEvent};
use crate::update::update;
use crate::view::render;

fn init_logging() {
    use std::sync::Mutex;
    use tracing_subscriber::EnvFilter;

    let log_val = match env::var("TIDE_LOG") {
        Ok(v) => v,
        Err(_) => return, // logging disabled
    };

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/tide.log")
    {
        Ok(f) => f,
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    launcher::launch_if_needed().await?;

    init_logging();

    let session_name = match env::args().nth(1) {
        Some(s) if !s.trim().is_empty() => s,
        _ => launcher::detect_session_name().await,
    };
    if session_name != launcher::TIDE_SESSION_NAME {
        anyhow::bail!(
            "tide only supports '{}' session (got '{}')",
            launcher::TIDE_SESSION_NAME,
            session_name
        );
    }
    info!(session = %session_name, "starting tide");

    install_panic_hook();
    enable_raw_mode()?;
    let _guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    debug!("connecting to tmux control mode");
    let mut tmux = match TmuxControl::new(&session_name).await {
        Ok(t) => {
            info!("tmux control connected");
            t
        }
        Err(err) => {
            error!(%err, "tmux control failed");
            return Err(err);
        }
    };

    // Exclude control client from window sizing calculations.
    // "no-output" flag tells tmux this client doesn't display output,
    // so it should be ignored for window-size decisions (tmux 3.2+).
    // Falls back silently on older tmux versions.
    let _ = tmux.send_command("refresh-client -f no-output").await;

    let (sidebar_pane_id, sidebar_window_id, home_pane_id) =
        detect_sidebar_context(&mut tmux).await?;

    let session_id = tmux
        .send_command("display-message -p '#{session_id}'")
        .await
        .map(|s| s.trim().to_string())
        .context("failed to detect session id")?;
    debug!(session_id, "detected session id");

    let mut model = Model::new(
        session_name.clone(),
        session_id,
        sidebar_pane_id.clone(),
        home_pane_id,
        sidebar_window_id,
    );

    // Save existing prefix+f binding before overwriting
    let prev_f_binding = tmux
        .send_command("list-keys -T prefix")
        .await
        .ok()
        .and_then(|output| {
            output
                .lines()
                .find(|line| {
                    line.split_whitespace()
                        .skip_while(|&w| w != "prefix")
                        .nth(1)
                        == Some("f")
                })
                .map(|s| s.trim().to_string())
        });

    // Bind prefix+f to jump back to sidebar pane
    let _ = tmux
        .send_command(&format!("bind-key f select-pane -t {}", sidebar_pane_id))
        .await;

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<Event>();
    spawn_input_thread(ui_tx);

    debug!("loading initial window list");
    let startup_cmds = match tmux.list_windows().await {
        Ok(windows) => {
            info!(count = windows.len(), "loaded windows");
            update(&mut model, Msg::WindowListLoaded(windows))
        }
        Err(err) => {
            error!(%err, "initial list_windows failed");
            model.error_message = Some(format!("initial list_windows failed: {err}"));
            vec![Cmd::Render]
        }
    };

    if !execute_commands(&mut model, &mut tmux, &mut terminal, startup_cmds).await {
        info!("startup commands requested quit");
        tmux.shutdown().await;
        return Ok(());
    }

    // Show restart notification
    if env::var("TIDE_RESTARTED").is_ok() {
        env::remove_var("TIDE_RESTARTED");
        model.info_message = Some("restarted".to_string());
        let _ = terminal.draw(|f| render(&model, f));
    }

    // Focus sidebar pane on startup
    let _ = tmux
        .send_command(&format!("select-pane -t {}", sidebar_pane_id))
        .await;

    // Initial AI process poll (detect immediately without waiting for first tick)
    let initial_poll = vec![Cmd::PollAiProcesses];
    if !execute_commands(&mut model, &mut tmux, &mut terminal, initial_poll).await {
        info!("initial AI poll requested quit");
        tmux.shutdown().await;
        return Ok(());
    }

    info!("entering main loop");

    let mut ai_poll_interval = tokio::time::interval(Duration::from_secs(1));
    ai_poll_interval.reset(); // skip immediate first tick (we already polled above)

    loop {
        if model.should_quit {
            debug!("should_quit is true, exiting");
            break;
        }

        tokio::select! {
            maybe_ui = ui_rx.recv() => {
                let Some(evt) = maybe_ui else {
                    warn!("ui channel closed");
                    break;
                };

                let cmds = match evt {
                    Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        update(&mut model, Msg::Key(key))
                    }
                    Event::Resize(_, _) => vec![Cmd::Render],
                    _ => Vec::new(),
                };

                if !cmds.is_empty() && !execute_commands(&mut model, &mut tmux, &mut terminal, cmds).await {
                    break;
                }
            }

            maybe_tmux = tmux.event_stream().recv() => {
                let Some(tmux_event) = maybe_tmux else {
                    warn!("tmux event stream closed");
                    model.error_message = Some("tmux event stream closed".to_string());
                    let _ = terminal.draw(|f| render(&model, f));
                    break;
                };
                debug!(?tmux_event, "received tmux event");

                let cmds = match tmux_event {
                    TmuxEvent::WindowAdd(_) | TmuxEvent::WindowClose(_) => {
                        update(&mut model, Msg::WindowChanged)
                    }
                    TmuxEvent::WindowRenamed(window_id, name) => {
                        update(&mut model, Msg::WindowRenamed { window_id, name })
                    }
                    TmuxEvent::SessionWindowChanged(ref sid, window_id)
                        if *sid == model.session_id =>
                    {
                        update(&mut model, Msg::WindowFocusChanged(window_id))
                    }
                    TmuxEvent::SessionWindowChanged(_, _) => Vec::new(),
                    TmuxEvent::SessionChanged(_, _) => vec![Cmd::ListWindows],
                    TmuxEvent::Error(err) => {
                        model.error_message = Some(err);
                        vec![Cmd::Render]
                    }
                };

                if !cmds.is_empty() && !execute_commands(&mut model, &mut tmux, &mut terminal, cmds).await {
                    break;
                }
            }

            _ = ai_poll_interval.tick() => {
                let cmds = vec![Cmd::PollAiProcesses];
                if !execute_commands(&mut model, &mut tmux, &mut terminal, cmds).await {
                    break;
                }
            }
        }
    }

    let restart = model.restart_requested;

    // Clean up AI border highlights before shutdown
    let cleanup_cmds = vec![Cmd::ResetAllBorders];
    let _ = execute_commands(&mut model, &mut tmux, &mut terminal, cleanup_cmds).await;

    // Restore previous prefix+f binding (or unbind if none existed)
    if let Some(ref binding) = prev_f_binding {
        let _ = tmux.send_command(binding).await;
    } else {
        let _ = tmux.send_command("unbind-key f").await;
    }
    tmux.shutdown().await;

    if restart {
        info!("restarting tide via exec");
        // TerminalGuard will drop and restore terminal before exec
        drop(_guard);
        let exe = env::current_exe().context("failed to get current exe path")?;
        let args: Vec<String> = env::args().collect();
        let err = std::process::Command::new(&exe)
            .args(&args[1..])
            .env("TIDE_RESTARTED", "1")
            .exec();
        // exec() only returns on error
        anyhow::bail!("exec failed: {err}");
    }

    info!("tide shut down");
    Ok(())
}

async fn detect_sidebar_context(tmux: &mut TmuxControl) -> Result<(String, String, String)> {
    let sidebar_pane_id = env::var("TMUX_PANE")
        .context("TMUX_PANE not set; tmuxide must run inside a tmux pane")?;
    if sidebar_pane_id.is_empty() {
        anyhow::bail!("TMUX_PANE is empty");
    }
    debug!(sidebar_pane_id, "detected sidebar pane");

    let sidebar_window_id = tmux
        .send_command(&format!(
            "display-message -t {sidebar_pane_id} -p '#{{window_id}}'"
        ))
        .await
        .map(|s| s.trim().to_string())
        .context("failed to detect sidebar window id")?;
    if sidebar_window_id.is_empty() {
        anyhow::bail!("detected empty sidebar window id");
    }
    debug!(sidebar_window_id, "detected sidebar window");

    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {sidebar_window_id} -F '#{{pane_id}}'"
        ))
        .await
        .context("failed to list panes in sidebar window")?;
    let home_pane_id = pane_list
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && *l != sidebar_pane_id)
        .unwrap_or("")
        .to_string();
    debug!(home_pane_id, "detected home pane");

    Ok((sidebar_pane_id, sidebar_window_id, home_pane_id))
}

fn spawn_input_thread(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("terminal read error: {err}");
                    break;
                }
            },
            Ok(false) => {}
            Err(err) => {
                eprintln!("terminal poll error: {err}");
                break;
            }
        }
    });
}
