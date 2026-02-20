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

use crate::cmd::Cmd;
use crate::model::Model;
use crate::msg::Msg;
use crate::tmux::{TmuxControl, TmuxEvent};
use crate::update::update;
use crate::view::render;

type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

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
            .args(["split-window", "-hb", "-l", "30", "--", "sh", "-c", &inner_cmd])
            .status();
        std::process::exit(status.map(|s| s.code().unwrap_or(0)).unwrap_or(1));
    }

    // Debug log for startup diagnostics (remove once stable)
    let dbg = |msg: &str| {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/tmuxide.log")
        {
            let _ = writeln!(f, "[{:?}] {}", std::time::Instant::now(), msg);
        }
    };

    let session_name = match env::args().nth(1) {
        Some(s) if !s.trim().is_empty() => s,
        _ => detect_session_name().await,
    };
    dbg(&format!("session: {session_name}"));

    install_panic_hook();
    enable_raw_mode()?;
    let _guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut model = Model::new(session_name.clone());

    dbg("connecting to tmux...");
    let mut tmux = match TmuxControl::new(&session_name).await {
        Ok(t) => {
            dbg("tmux control connected");
            t
        }
        Err(err) => {
            dbg(&format!("tmux control failed: {err}"));
            return Err(err);
        }
    };

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<Event>();
    spawn_input_thread(ui_tx);

    dbg("calling list_windows...");
    let startup_cmds = match tmux.list_windows().await {
        Ok(windows) => {
            dbg(&format!("list_windows ok: {} windows", windows.len()));
            update(&mut model, Msg::WindowListLoaded(windows))
        }
        Err(err) => {
            dbg(&format!("list_windows failed: {err}"));
            model.error_message = Some(format!("initial list_windows failed: {err}"));
            vec![Cmd::Render]
        }
    };

    if !execute_commands(&mut model, &mut tmux, &mut terminal, startup_cmds).await {
        dbg("startup commands requested quit");
        tmux.shutdown().await;
        return Ok(());
    }
    dbg("entering main loop");

    loop {
        if model.should_quit {
            dbg("should_quit is true");
            break;
        }

        tokio::select! {
            maybe_ui = ui_rx.recv() => {
                let Some(evt) = maybe_ui else {
                    dbg("ui channel closed");
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
                    dbg("tmux event stream closed");
                    model.error_message = Some("tmux event stream closed".to_string());
                    let _ = terminal.draw(|f| render(&model, f));
                    break;
                };
                dbg(&format!("tmux event: {:?}", tmux_event));

                let cmds = match tmux_event {
                    TmuxEvent::WindowAdd(id) => update(&mut model, Msg::WindowAdded(id)),
                    TmuxEvent::WindowClose(id) => update(&mut model, Msg::WindowClosed(id)),
                    TmuxEvent::WindowRenamed(id, name) => update(&mut model, Msg::WindowRenamed(id, name)),
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

    tmux.shutdown().await;
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
            Cmd::SelectWindow { id } => {
                let cmd_str = format!("select-window -t {id}");
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    model.error_message = Some(format!("select-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::FocusRightPane => {
                if let Err(err) = tmux.send_command("select-pane -R").await {
                    model.error_message = Some(format!("select-pane: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::NewWindow { name } => {
                let new_cmd = format!("new-window -n {}", quote_tmux(&name));
                if let Err(err) = tmux.send_command(&new_cmd).await {
                    model.error_message = Some(format!("new-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::RenameWindow { id, name } => {
                let cmd_str = format!("rename-window -t {} {}", id, quote_tmux(&name));
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    model.error_message = Some(format!("rename-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::CloseWindow { id } => {
                let cmd_str = format!("kill-window -t {id}");
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    model.error_message = Some(format!("kill-window: {err}"));
                    queue.push_front(Cmd::Render);
                }
            }
            Cmd::ListWindows => match tmux.list_windows().await {
                Ok(windows) => {
                    let follow_up = update(model, Msg::WindowListLoaded(windows));
                    for c in follow_up.into_iter().rev() {
                        queue.push_front(c);
                    }
                }
                Err(err) => {
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
