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
use tide::cmd::Cmd;
use tide::execute::{execute_commands, AppTerminal};
use tide::launcher;
use tide::metrics::snapshot_tmux_metrics;
use tide::model::Model;
use tide::msg::Msg;
use tide::tmux::commands;
use tide::tmux::{TmuxControl, TmuxEvent};
use tide::update::update;
use tide::view::render;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

#[cfg(test)]
use tide::tmux::WindowInfo;

const AI_POLL_INTERVAL_MS: u64 = 500;
const PREVIEW_DEBOUNCE_MS: u64 = 50;
const INPUT_POLL_MS: u64 = 100;

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

struct App {
    model: Model,
    tmux: TmuxControl,
    terminal: AppTerminal,
    ui_rx: mpsc::UnboundedReceiver<Event>,
    pending_preview: Option<String>,
}

impl App {
    async fn bootstrap(session_name: String) -> Result<(Self, Option<String>)> {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, CursorHide)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let mut tmux = connect_tmux_control(&session_name).await?;
        let (sidebar_pane_id, sidebar_window_id, home_pane_id) = detect_sidebar_context().await?;

        let session_id = tmux
            .send_command("display-message -p '#{session_id}'")
            .await
            .map(|s| s.trim().to_string())
            .context("failed to detect session id")?;
        debug!(session_id, "detected session id");

        let mut model = Model::new(
            session_name,
            session_id,
            sidebar_pane_id.clone(),
            home_pane_id,
            sidebar_window_id,
        );
        model.terminal_size = crossterm::terminal::size().unwrap_or((80, 24));

        let prev_f_binding = capture_prefix_f_binding(&mut tmux).await;
        bind_prefix_f_to_sidebar(&mut tmux, &sidebar_pane_id).await;

        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<Event>();
        spawn_input_thread(ui_tx);

        Ok((
            Self {
                model,
                tmux,
                terminal,
                ui_rx,
                pending_preview: None,
            },
            prev_f_binding,
        ))
    }

    async fn run(&mut self) -> bool {
        if !run_startup_render(&mut self.model, &mut self.tmux, &mut self.terminal).await {
            info!("startup commands requested quit");
            return false;
        }

        if env::var("TIDE_RESTARTED").is_ok() {
            env::remove_var("TIDE_RESTARTED");
            self.model.info_message = Some("restarted".to_string());
            let _ = self.terminal.draw(|f| render(&self.model, f));
        }

        let _ = self
            .tmux
            .send_command(&commands::select_pane(&self.model.sidebar.pane_id))
            .await;

        if !run_initial_ai_poll(&mut self.model, &mut self.tmux, &mut self.terminal).await {
            info!("initial AI poll requested quit");
            return false;
        }

        info!("entering main loop");

        let mut ai_poll_interval =
            tokio::time::interval(Duration::from_millis(AI_POLL_INTERVAL_MS));
        ai_poll_interval.reset();
        ai_poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let preview_sleep = tokio::time::sleep(Duration::from_secs(86400));
        tokio::pin!(preview_sleep);

        loop {
            if self.model.should_quit {
                debug!("should_quit is true, exiting");
                break;
            }

            tokio::select! {
                maybe_ui = self.ui_rx.recv() => {
                    if !handle_ui_tick(
                        &mut self.model,
                        &mut self.tmux,
                        &mut self.terminal,
                        &mut self.ui_rx,
                        maybe_ui,
                        &mut self.pending_preview,
                        preview_sleep.as_mut(),
                    ).await {
                        break;
                    }
                }

                () = &mut preview_sleep, if self.pending_preview.is_some() => {
                    if !handle_preview_tick(
                        &mut self.model,
                        &mut self.tmux,
                        &mut self.terminal,
                        &mut self.pending_preview,
                    ).await {
                        break;
                    }
                }

                maybe_tmux = self.tmux.event_stream().recv() => {
                    let Some(tmux_event) = maybe_tmux else {
                        warn!("tmux event stream closed");
                        self.model.error_message = Some("tmux event stream closed".to_string());
                        let _ = self.terminal.draw(|f| render(&self.model, f));
                        break;
                    };

                    let cmds = {
                        let tmux_rx = self.tmux.event_stream();
                        process_tmux_batch(&mut self.model, tmux_rx, tmux_event)
                    };

                    if !execute_if_any(&mut self.model, &mut self.tmux, &mut self.terminal, cmds).await {
                        break;
                    }
                }

                _ = ai_poll_interval.tick() => {
                    if !handle_ai_poll_tick(&mut self.model, &mut self.tmux, &mut self.terminal).await {
                        break;
                    }
                }
            }
        }

        true
    }

    async fn shutdown(&mut self, prev_f_binding: Option<&str>) {
        let cleanup_cmds = vec![Cmd::RestorePreview, Cmd::ResetAllBorders];
        let _ = execute_commands(
            &mut self.model,
            &mut self.tmux,
            &mut self.terminal,
            cleanup_cmds,
        )
        .await;
        restore_prefix_f_binding(&mut self.tmux, prev_f_binding).await;
        self.tmux.shutdown().await;
        let metrics = snapshot_tmux_metrics();
        info!(
            pane_output_dropped = metrics.pane_output_dropped,
            coalesced_resync_deferred = metrics.coalesced_resync_deferred,
            coalesced_resync_flushed = metrics.coalesced_resync_flushed,
            command_failures = metrics.command_failures,
            batch_reconciles = metrics.batch_reconciles,
            "tmux metrics"
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

    let (mut app, prev_f_binding) = App::bootstrap(session_name.clone()).await?;
    let _ = app.run().await;
    let restart = app.model.restart_requested;
    app.shutdown(prev_f_binding.as_deref()).await;

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

async fn detect_sidebar_context() -> Result<(String, String, String)> {
    let sidebar_pane_id =
        env::var("TMUX_PANE").context("TMUX_PANE not set; tide must run inside a tmux pane")?;
    if sidebar_pane_id.is_empty() {
        anyhow::bail!("TMUX_PANE is empty");
    }
    debug!(sidebar_pane_id, "detected sidebar pane");

    let sidebar_window_id = tokio::process::Command::new("tmux")
        .args([
            "display-message",
            "-t",
            &sidebar_pane_id,
            "-p",
            "#{window_id}",
        ])
        .output()
        .await
        .context("failed to detect sidebar window id")?;
    let sidebar_window_id = String::from_utf8_lossy(&sidebar_window_id.stdout)
        .trim()
        .to_string();
    if sidebar_window_id.is_empty() {
        anyhow::bail!("detected empty sidebar window id");
    }

    debug!(sidebar_window_id, "detected sidebar window");

    let pane_list = tokio::process::Command::new("tmux")
        .args(["list-panes", "-t", &sidebar_window_id, "-F", "#{pane_id}"])
        .output()
        .await
        .context("failed to list panes in sidebar window")?;
    let home_pane_id = String::from_utf8_lossy(&pane_list.stdout)
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && *l != sidebar_pane_id)
        .unwrap_or("")
        .to_string();
    debug!(home_pane_id, "detected home pane");

    Ok((sidebar_pane_id, sidebar_window_id, home_pane_id))
}

async fn connect_tmux_control(session_name: &str) -> Result<TmuxControl> {
    debug!("connecting to tmux control mode");
    let mut tmux = match TmuxControl::new(session_name).await {
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
    Ok(tmux)
}

async fn capture_prefix_f_binding(tmux: &mut TmuxControl) -> Option<String> {
    tmux.send_command("list-keys -T prefix")
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
        })
}

async fn bind_prefix_f_to_sidebar(tmux: &mut TmuxControl, sidebar_pane_id: &str) {
    let _ = tmux
        .send_command(&format!("bind-key f select-pane -t {}", sidebar_pane_id))
        .await;
}

async fn restore_prefix_f_binding(tmux: &mut TmuxControl, binding: Option<&str>) {
    if let Some(binding) = binding {
        let _ = tmux.send_command(binding).await;
    } else {
        let _ = tmux.send_command("unbind-key f").await;
    }
}

async fn run_startup_render(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> bool {
    debug!("loading initial window list");
    let startup_cmds = match tmux.list_windows().await {
        Ok(windows) => {
            info!(count = windows.len(), "loaded windows");
            update(model, Msg::WindowListLoaded(windows))
        }
        Err(err) => {
            error!(%err, "initial list_windows failed");
            model.error_message = Some(format!("initial list_windows failed: {err}"));
            vec![Cmd::Render]
        }
    };
    execute_commands(model, tmux, terminal, startup_cmds).await
}

async fn run_initial_ai_poll(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> bool {
    // Initial AI process poll (detect immediately without waiting for first tick).
    execute_commands(model, tmux, terminal, vec![Cmd::PollAiProcesses]).await
}

fn spawn_input_thread(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(INPUT_POLL_MS)) {
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

fn process_ui_batch(
    model: &mut Model,
    ui_rx: &mut mpsc::UnboundedReceiver<Event>,
    first_evt: Event,
) -> Vec<Cmd> {
    let mut cmds = process_ui_event(model, first_evt);

    // Drain all pending UI events to batch rapid input (key repeat).
    while let Ok(evt) = ui_rx.try_recv() {
        cmds.extend(process_ui_event(model, evt));
    }

    // Coalesce: keep only the last PreviewWindow, deduplicate Renders.
    coalesce_commands(cmds)
}

async fn handle_ui_tick(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ui_rx: &mut mpsc::UnboundedReceiver<Event>,
    maybe_ui: Option<Event>,
    pending_preview: &mut Option<String>,
    preview_sleep: std::pin::Pin<&mut tokio::time::Sleep>,
) -> bool {
    let Some(evt) = maybe_ui else {
        warn!("ui channel closed");
        return false;
    };

    let mut cmds = process_ui_batch(model, ui_rx, evt);
    apply_preview_debounce(&mut cmds, pending_preview, preview_sleep);
    execute_if_any(model, tmux, terminal, cmds).await
}

async fn handle_preview_tick(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    pending_preview: &mut Option<String>,
) -> bool {
    if let Some(id) = pending_preview.take() {
        return execute_commands(model, tmux, terminal, vec![Cmd::PreviewWindow { id }]).await;
    }
    true
}

async fn handle_ai_poll_tick(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> bool {
    execute_commands(model, tmux, terminal, vec![Cmd::PollAiProcesses]).await
}

async fn execute_if_any(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cmds: Vec<Cmd>,
) -> bool {
    if cmds.is_empty() {
        true
    } else {
        execute_commands(model, tmux, terminal, cmds).await
    }
}

fn apply_preview_debounce(
    cmds: &mut Vec<Cmd>,
    pending_preview: &mut Option<String>,
    mut sleep: std::pin::Pin<&mut tokio::time::Sleep>,
) {
    // Defer preview with debounce — sidebar renders immediately
    // but the expensive tmux pane-swap waits until input settles.
    if let Some(id) = extract_deferred_preview(cmds) {
        *pending_preview = Some(id);
        sleep
            .as_mut()
            .reset(tokio::time::Instant::now() + Duration::from_millis(PREVIEW_DEBOUNCE_MS));
    } else if !cmds.is_empty() {
        // Non-cursor action cancels any pending deferred preview.
        *pending_preview = None;
    }
}

fn process_tmux_event(model: &mut Model, tmux_event: TmuxEvent) -> Vec<Cmd> {
    match tmux_event {
        TmuxEvent::WindowAdd(_) | TmuxEvent::WindowClose(_) => update(model, Msg::WindowChanged),
        TmuxEvent::WindowRenamed(window_id, name) => {
            update(model, Msg::WindowRenamed { window_id, name })
        }
        TmuxEvent::SessionWindowChanged(ref sid, window_id) if *sid == model.session_id => {
            update(model, Msg::WindowFocusChanged(window_id))
        }
        TmuxEvent::LayoutChange(window_id) if window_id == model.sidebar.window_id => {
            if let Some(deadline) = model.sidebar.ignore_layout_change_until {
                if std::time::Instant::now() < deadline {
                    return vec![Cmd::EnsureSidebarWidth];
                }
                model.sidebar.ignore_layout_change_until = None;
            }
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
            if pane_id != model.sidebar.pane_id {
                *model.ai.output_counts.entry(pane_id).or_insert(0) += 1;
            }
            Vec::new()
        }
        TmuxEvent::Error(err) => {
            model.error_message = Some(err);
            vec![Cmd::Render]
        }
    }
}

fn process_tmux_batch(
    model: &mut Model,
    tmux_rx: &mut mpsc::Receiver<TmuxEvent>,
    first_event: TmuxEvent,
) -> Vec<Cmd> {
    debug!(?first_event, "received tmux event");
    let mut cmds = process_tmux_event(model, first_event);

    while let Ok(event) = tmux_rx.try_recv() {
        debug!(?event, "draining tmux event");
        cmds.extend(process_tmux_event(model, event));
    }

    coalesce_commands(cmds)
}

/// Coalesce commands from batched UI events:
/// keep only the last PreviewWindow, deduplicate refresh commands,
/// and collapse multiple Renders into one.
fn coalesce_commands(cmds: Vec<Cmd>) -> Vec<Cmd> {
    let last_preview_idx = cmds
        .iter()
        .rposition(|c| matches!(c, Cmd::PreviewWindow { .. }));

    let mut result = Vec::new();
    let mut has_render = false;
    let mut has_list_windows = false;
    let mut has_ensure_sidebar_width = false;
    let mut has_validate_sidebar_panes = false;

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
            Cmd::ListWindows => {
                if !has_list_windows {
                    has_list_windows = true;
                    result.push(cmd);
                }
            }
            Cmd::EnsureSidebarWidth => {
                if !has_ensure_sidebar_width {
                    has_ensure_sidebar_width = true;
                    result.push(cmd);
                }
            }
            Cmd::ValidateSidebarPanes => {
                if !has_validate_sidebar_panes {
                    has_validate_sidebar_panes = true;
                    result.push(cmd);
                }
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home".to_string(),
            "@home".to_string(),
        )
    }

    fn window(id: &str, index: usize, name: &str) -> WindowInfo {
        WindowInfo {
            id: id.to_string(),
            index,
            name: name.to_string(),
            active: false,
        }
    }

    #[test]
    fn layout_change_is_suppressed_while_helper_is_running() {
        let mut model = test_model();
        model.sidebar.ignore_layout_change_until =
            Some(Instant::now() + Duration::from_millis(500));

        let cmds = process_tmux_event(&mut model, TmuxEvent::LayoutChange("@home".to_string()));

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::EnsureSidebarWidth));
        assert!(model.sidebar.ignore_layout_change_until.is_some());
    }

    #[test]
    fn layout_change_revalidates_after_suppression_expires() {
        let mut model = test_model();
        model.sidebar.ignore_layout_change_until = Some(Instant::now() - Duration::from_millis(1));

        let cmds = process_tmux_event(&mut model, TmuxEvent::LayoutChange("@home".to_string()));

        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::EnsureSidebarWidth));
        assert!(matches!(cmds[1], Cmd::ValidateSidebarPanes));
        assert!(model.sidebar.ignore_layout_change_until.is_none());
    }

    #[test]
    fn process_ui_batch_keeps_last_preview_and_single_render() {
        let mut model = test_model();
        let _ = update(
            &mut model,
            Msg::WindowListLoaded(vec![
                window("@1", 1, "one"),
                window("@2", 2, "two"),
                window("@3", 3, "three"),
            ]),
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();
        tx.send(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .unwrap();

        let cmds = process_ui_batch(
            &mut model,
            &mut rx,
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::PreviewWindow { ref id } if id == "@3"));
        assert!(matches!(cmds[1], Cmd::Render));
        assert_eq!(model.cursor(), 2);
    }

    #[test]
    fn apply_preview_debounce_cancels_pending_preview_on_non_cursor_action() {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async {
            let mut pending_preview = Some("@2".to_string());
            let sleep = tokio::time::sleep(Duration::from_secs(86400));
            tokio::pin!(sleep);
            let mut cmds = vec![Cmd::ListWindows, Cmd::Render];

            apply_preview_debounce(&mut cmds, &mut pending_preview, sleep.as_mut());

            assert_eq!(pending_preview, None);
            assert_eq!(cmds.len(), 2);
            assert!(matches!(cmds[0], Cmd::ListWindows));
            assert!(matches!(cmds[1], Cmd::Render));
        });
    }

    #[test]
    fn process_tmux_batch_coalesces_redundant_refresh_commands() {
        let mut model = test_model();
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TmuxEvent::WindowClose("@2".to_string()))
            .expect("queue close event");
        tx.try_send(TmuxEvent::SessionChanged("$1".to_string(), "s".to_string()))
            .expect("queue session event");

        let cmds = process_tmux_batch(&mut model, &mut rx, TmuxEvent::WindowAdd("@1".to_string()));

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::ListWindows));
    }

    #[test]
    fn process_tmux_batch_coalesces_sidebar_validation_commands() {
        let mut model = test_model();
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(TmuxEvent::LayoutChange("@home".to_string()))
            .expect("queue layout event");

        let cmds = process_tmux_batch(
            &mut model,
            &mut rx,
            TmuxEvent::LayoutChange("@home".to_string()),
        );

        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::EnsureSidebarWidth));
        assert!(matches!(cmds[1], Cmd::ValidateSidebarPanes));
    }
}
