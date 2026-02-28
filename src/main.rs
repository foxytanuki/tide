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
use crossterm::cursor::{Hide as CursorHide, Show as CursorShow};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
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
    let _guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, CursorHide)?;
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
    // "ignore-size" tells tmux not to use this client for window-size
    // decisions (tmux 3.4+). We avoid "no-output" because it also
    // suppresses %output events needed for AI activity detection.
    // Falls back silently on older tmux versions.
    let _ = tmux.send_command("refresh-client -f ignore-size").await;

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
    model.terminal_size = crossterm::terminal::size().unwrap_or((80, 24));

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

    let mut ai_poll_interval = tokio::time::interval(Duration::from_millis(500));
    ai_poll_interval.reset(); // skip immediate first tick (we already polled above)

    let mut pending_preview: Option<String> = None;
    let preview_sleep = tokio::time::sleep(Duration::from_secs(86400));
    tokio::pin!(preview_sleep);

    loop {
        if model.should_quit {
            debug!("should_quit is true, exiting");
            break;
        }

        tokio::select! {
            // Prioritise user input over background tasks so keystrokes
            // are never starved by AI poll or tmux event processing.
            biased;

            maybe_ui = ui_rx.recv() => {
                let Some(evt) = maybe_ui else {
                    warn!("ui channel closed");
                    break;
                };

                let mut cmds = process_ui_event(&mut model, evt);

                // Drain all pending UI events to batch rapid input (key repeat)
                while let Ok(evt) = ui_rx.try_recv() {
                    cmds.extend(process_ui_event(&mut model, evt));
                }

                // Coalesce: keep only the last PreviewWindow, deduplicate Renders
                let mut cmds = coalesce_commands(cmds);

                // Defer preview with debounce — sidebar renders immediately
                // but the expensive tmux pane-swap waits until input settles.
                if let Some(id) = extract_deferred_preview(&mut cmds) {
                    pending_preview = Some(id);
                    preview_sleep.as_mut().reset(
                        tokio::time::Instant::now() + Duration::from_millis(50),
                    );
                } else if !cmds.is_empty() {
                    // Non-cursor action cancels any pending deferred preview
                    pending_preview = None;
                }

                if !cmds.is_empty() && !execute_commands(&mut model, &mut tmux, &mut terminal, cmds).await {
                    break;
                }
            }

            // Debounced preview: fires after cursor movement settles
            () = &mut preview_sleep, if pending_preview.is_some() => {
                if let Some(id) = pending_preview.take() {
                    let cmds = vec![Cmd::PreviewWindow { id }];
                    if !execute_commands(&mut model, &mut tmux, &mut terminal, cmds).await {
                        break;
                    }
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
                    TmuxEvent::LayoutChange(window_id)
                        if window_id == model.sidebar_window_id =>
                    {
                        vec![Cmd::EnsureSidebarWidth, Cmd::ValidateSidebarPanes]
                    }
                    TmuxEvent::LayoutChange(_) => Vec::new(),
                    TmuxEvent::SessionWindowChanged(_, _) => Vec::new(),
                    TmuxEvent::SessionChanged(_, _) => vec![Cmd::ListWindows],
                    TmuxEvent::PaneOutput(pane_id) => {
                        // Hot-path exception: update output counter directly
                        // instead of going through Msg→update→Cmd cycle.
                        // %output fires at very high frequency during streaming;
                        // routing through TEA would create unnecessary overhead.
                        // Skip sidebar pane's own output to avoid self-triggering.
                        if pane_id != model.sidebar_pane_id {
                            *model.ai_output_counts.entry(pane_id).or_insert(0) += 1;
                        }
                        Vec::new()
                    }
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

    // Restore preview layout and clean up AI border highlights before shutdown
    let cleanup_cmds = vec![Cmd::RestorePreview, Cmd::ResetAllBorders];
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
    let sidebar_pane_id =
        env::var("TMUX_PANE").context("TMUX_PANE not set; tide must run inside a tmux pane")?;
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

/// Process a single UI event into commands.
fn process_ui_event(model: &mut Model, evt: Event) -> Vec<Cmd> {
    match evt {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            update(model, Msg::Key(key))
        }
        Event::Resize(w, h) => {
            model.terminal_size = (w, h);
            vec![Cmd::EnsureSidebarWidth, Cmd::Render]
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                update(model, Msg::MouseClick { row: mouse.row })
            }
            MouseEventKind::ScrollUp => update(model, Msg::MouseScrollUp),
            MouseEventKind::ScrollDown => update(model, Msg::MouseScrollDown),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Coalesce commands from batched UI events:
/// keep only the last PreviewWindow and collapse multiple Renders into one.
fn coalesce_commands(cmds: Vec<Cmd>) -> Vec<Cmd> {
    if cmds.len() <= 2 {
        return cmds;
    }

    let last_preview_idx = cmds
        .iter()
        .rposition(|c| matches!(c, Cmd::PreviewWindow { .. }));

    let mut result = Vec::new();
    let mut has_render = false;

    for (i, cmd) in cmds.into_iter().enumerate() {
        match &cmd {
            Cmd::PreviewWindow { .. } => {
                if Some(i) == last_preview_idx {
                    result.push(cmd);
                }
            }
            Cmd::Render => {
                has_render = true;
            }
            _ => {
                result.push(cmd);
            }
        }
    }

    if has_render {
        result.push(Cmd::Render);
    }

    result
}

/// Extract PreviewWindow for deferred execution if the command list is a
/// pure cursor movement (only PreviewWindow + Render). Returns the window ID.
fn extract_deferred_preview(cmds: &mut Vec<Cmd>) -> Option<String> {
    let dominated_by_cursor = cmds
        .iter()
        .all(|c| matches!(c, Cmd::PreviewWindow { .. } | Cmd::Render));
    if !dominated_by_cursor {
        return None;
    }

    if let Some(idx) = cmds
        .iter()
        .rposition(|c| matches!(c, Cmd::PreviewWindow { .. }))
    {
        if let Cmd::PreviewWindow { id } = cmds.remove(idx) {
            return Some(id);
        }
    }
    None
}
