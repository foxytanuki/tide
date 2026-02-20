mod cmd;
mod model;
mod msg;
mod tmux;
mod tree;
mod update;
mod view;

use std::collections::VecDeque;
use std::env;
use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::cmd::Cmd;
use crate::model::Model;
use crate::msg::Msg;
use crate::tmux::{TmuxControl, TmuxEvent};
use crate::update::update;
use crate::view::render;

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

fn init_logging() {
    use std::sync::Mutex;
    use tracing_subscriber::EnvFilter;

    let log_val = match env::var("TMUXIDE_LOG") {
        Ok(v) => v,
        Err(_) => return, // logging disabled
    };

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/tmuxide.log")
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
    // If inside tmux but not already in a sidebar pane, re-launch as split
    if env::var("TMUX").is_ok() && env::var("TMUXIDE_SIDEBAR").is_err() {
        let exe = env::current_exe()
            .unwrap_or_else(|_| "tmuxide".into())
            .display()
            .to_string();
        let args: Vec<String> = env::args().skip(1).collect();
        let mut inner_cmd = format!("TMUXIDE_SIDEBAR=1 {exe}");
        for arg in &args {
            inner_cmd.push(' ');
            inner_cmd.push_str(arg);
        }
        let status = std::process::Command::new("tmux")
            .args([
                "split-window",
                "-hb",
                "-l",
                "30",
                "--",
                "sh",
                "-c",
                &inner_cmd,
            ])
            .status();
        std::process::exit(status.map(|s| s.code().unwrap_or(0)).unwrap_or(1));
    }

    init_logging();

    let session_name = match env::args().nth(1) {
        Some(s) if !s.trim().is_empty() => s,
        _ => detect_session_name().await,
    };
    info!(session = %session_name, "starting tmuxide");

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

    // Detect sidebar pane, its window, and sibling (home) pane
    let sidebar_pane_id = env::var("TMUX_PANE").unwrap_or_default();
    debug!(sidebar_pane_id, "detected sidebar pane");

    let sidebar_window_id = tmux
        .send_command(&format!(
            "display-message -t {sidebar_pane_id} -p '#{{window_id}}'"
        ))
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    debug!(sidebar_window_id, "detected sidebar window");

    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {sidebar_window_id} -F '#{{pane_id}}'"
        ))
        .await
        .unwrap_or_default();
    let home_pane_id = pane_list
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && *l != sidebar_pane_id)
        .unwrap_or("")
        .to_string();
    debug!(home_pane_id, "detected home pane");

    let mut model = Model::new(
        session_name.clone(),
        sidebar_pane_id.clone(),
        home_pane_id,
        sidebar_window_id,
    );

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
    info!("entering main loop");

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
                    TmuxEvent::WindowAdd(id) => update(&mut model, Msg::WindowAdded(id)),
                    TmuxEvent::WindowClose(id) => update(&mut model, Msg::WindowClosed(id)),
                    TmuxEvent::WindowRenamed(id, name) => update(&mut model, Msg::WindowRenamed(id, name)),
                    TmuxEvent::SessionWindowChanged(_, window_id) => {
                        update(&mut model, Msg::WindowFocusChanged(window_id))
                    }
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
        }
    }

    let _ = tmux.send_command("unbind-key f").await;
    tmux.shutdown().await;
    info!("tmuxide shut down");
    Ok(())
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

async fn execute_commands(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut AppTerminal,
    cmds: Vec<Cmd>,
) -> bool {
    let mut queue: VecDeque<Cmd> = cmds.into();

    while let Some(cmd) = queue.pop_front() {
        match cmd {
            Cmd::PreviewWindow {
                id: target_window_id,
            } => {
                use crate::model::PreviewState;

                // If previewing and target is the original window, restore
                if let PreviewState::Previewing {
                    ref original_window_id,
                    ..
                } = model.preview
                {
                    if target_window_id == *original_window_id {
                        debug!(target = %target_window_id, "preview target is original, restoring");
                        queue.push_front(Cmd::Render);
                        queue.push_front(Cmd::RestorePreview);
                        continue;
                    }
                }

                // Already on this window, no-op
                if target_window_id == model.sidebar_window_id {
                    continue;
                }

                debug!(target = %target_window_id, "previewing window");

                // Record original state if starting fresh preview
                let (orig_window, orig_home) = match &model.preview {
                    PreviewState::Home => {
                        (model.sidebar_window_id.clone(), model.home_pane_id.clone())
                    }
                    PreviewState::Previewing {
                        original_window_id,
                        original_home_pane_id,
                    } => (original_window_id.clone(), original_home_pane_id.clone()),
                };

                // Move sidebar to target window
                let join_cmd = format!(
                    "join-pane -dfhb -l 30 -s {} -t {}",
                    model.sidebar_pane_id, target_window_id
                );
                if let Err(err) = tmux.send_command(&join_cmd).await {
                    warn!(%err, "preview join-pane failed");
                    model.error_message = Some(format!("preview join-pane: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Switch active window to target
                model.ignore_next_window_change = true;
                if let Err(err) = tmux
                    .send_command(&format!("select-window -t {}", target_window_id))
                    .await
                {
                    warn!(%err, "preview select-window failed");
                    model.error_message = Some(format!("preview select-window: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Focus sidebar pane
                let _ = tmux
                    .send_command(&format!("select-pane -t {}", model.sidebar_pane_id))
                    .await;

                // Update home_pane_id to a pane in the target window
                let pane_list = tmux
                    .send_command(&format!(
                        "list-panes -t {} -F '#{{pane_id}}'",
                        target_window_id
                    ))
                    .await
                    .unwrap_or_default();
                let new_home = pane_list
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty() && *l != model.sidebar_pane_id)
                    .unwrap_or("")
                    .to_string();
                if !new_home.is_empty() {
                    model.home_pane_id = new_home.clone();
                }

                debug!(
                    sidebar_window = %target_window_id,
                    home_pane = %model.home_pane_id,
                    orig_window = %orig_window,
                    "preview active"
                );

                model.sidebar_window_id = target_window_id;
                model.preview = PreviewState::Previewing {
                    original_window_id: orig_window,
                    original_home_pane_id: orig_home,
                };
            }
            Cmd::RestorePreview => {
                use crate::model::PreviewState;
                if let PreviewState::Previewing {
                    ref original_window_id,
                    ref original_home_pane_id,
                } = model.preview
                {
                    let orig_window = original_window_id.clone();
                    let orig_home = original_home_pane_id.clone();

                    debug!(orig_window = %orig_window, "restoring preview");

                    // Move sidebar back to original window
                    let join_cmd = format!(
                        "join-pane -dfhb -l 30 -s {} -t {}",
                        model.sidebar_pane_id, orig_window
                    );
                    let _ = tmux.send_command(&join_cmd).await;

                    // Switch back to original window
                    model.ignore_next_window_change = true;
                    let _ = tmux
                        .send_command(&format!("select-window -t {}", orig_window))
                        .await;

                    // Focus sidebar
                    let _ = tmux
                        .send_command(&format!("select-pane -t {}", model.sidebar_pane_id))
                        .await;

                    model.sidebar_window_id = orig_window;
                    model.home_pane_id = orig_home;
                    model.preview = PreviewState::Home;
                }
            }
            Cmd::FocusRightPane => {
                debug!("focusing right pane");
                if let Err(err) = tmux.send_command("select-pane -R").await {
                    warn!(%err, "select-pane -R failed");
                    model.error_message = Some(format!("select-pane: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::NewWindow { name } => {
                debug!(name, "creating new window");
                let new_cmd = format!("new-window -d -n {}", quote_tmux(&name));
                if let Err(err) = tmux.send_command(&new_cmd).await {
                    warn!(%err, "new-window failed");
                    model.error_message = Some(format!("new-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::RenameWindow { id, name } => {
                debug!(id, name, "renaming window");
                let cmd_str = format!("rename-window -t {} {}", id, quote_tmux(&name));
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "rename-window failed");
                    model.error_message = Some(format!("rename-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::CloseWindow { id } => {
                // If sidebar is in the window being closed, restore preview first
                if let crate::model::PreviewState::Previewing { .. } = &model.preview {
                    if model.sidebar_window_id == id {
                        debug!(id, "closing previewed window, restoring first");
                        queue.push_front(Cmd::CloseWindow { id });
                        queue.push_front(Cmd::RestorePreview);
                        continue;
                    }
                }
                debug!(id, "closing window");
                let cmd_str = format!("kill-window -t {id}");
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "kill-window failed");
                    model.error_message = Some(format!("kill-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::FollowToWindow {
                window_id: target_window_id,
            } => {
                if target_window_id == model.sidebar_window_id {
                    continue;
                }

                info!(target = %target_window_id, "following to window");

                // Move sidebar pane to the new window (left split, 30 cols, don't focus)
                let join_cmd = format!(
                    "join-pane -dfhb -l 30 -s {} -t {}",
                    model.sidebar_pane_id, target_window_id
                );
                if let Err(err) = tmux.send_command(&join_cmd).await {
                    warn!(%err, "follow join-pane failed");
                    model.error_message = Some(format!("follow join-pane: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Find new home pane (any pane in the target window that isn't the sidebar)
                let pane_list = tmux
                    .send_command(&format!(
                        "list-panes -t {} -F '#{{pane_id}}'",
                        target_window_id
                    ))
                    .await
                    .unwrap_or_default();
                let new_home = pane_list
                    .lines()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty() && *l != model.sidebar_pane_id)
                    .unwrap_or("")
                    .to_string();

                if !new_home.is_empty() {
                    model.home_pane_id = new_home;
                }

                debug!(
                    sidebar_window = %target_window_id,
                    home_pane = %model.home_pane_id,
                    "follow complete"
                );
                model.sidebar_window_id = target_window_id;
            }
            Cmd::ListWindows => match tmux.list_windows().await {
                Ok(windows) => {
                    debug!(count = windows.len(), "refreshed window list");
                    let follow_up = update(model, Msg::WindowListLoaded(windows));
                    for c in follow_up.into_iter().rev() {
                        queue.push_front(c);
                    }
                }
                Err(err) => {
                    warn!(%err, "list-windows failed");
                    model.error_message = Some(format!("list-windows: {err}"));
                    queue.push_front(Cmd::Render);
                }
            },
            Cmd::Render => {
                if let Err(err) = terminal.draw(|f| render(model, f)) {
                    model.error_message = Some(format!("render: {err}"));
                }
            }
            Cmd::Quit => return false,
            Cmd::Batch(batch) => {
                for c in batch.into_iter().rev() {
                    queue.push_front(c);
                }
            }
        }
    }

    true
}

async fn detect_session_name() -> String {
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

/// Escape a string for use in tmux command arguments.
/// tmux control mode uses its own parser (not shell). Double-quote and escape inside.
fn quote_tmux(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for c in input.chars() {
        match c {
            '"' | '\\' | '$' | '#' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
