use std::env;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::cursor::Hide as CursorHide;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::cmd::Cmd;
use crate::execute::{execute_commands, AppTerminal};
use crate::metrics::snapshot_tmux_metrics;
use crate::model::Model;
use crate::msg::Msg;
use crate::tmux::commands;
use crate::tmux::{TmuxControl, TmuxEvent};
use crate::update::update;
use crate::view::render;

#[cfg(test)]
use crate::tmux::WindowInfo;

const AI_POLL_INTERVAL_MS: u64 = 500;
const PREVIEW_DEBOUNCE_MS: u64 = 50;
const INPUT_POLL_MS: u64 = 100;
const MAX_TMUX_EVENTS_PER_BATCH: usize = 64;
const TERMINAL_RESPONSE_FILTER_MS: u64 = 2_000;
const RESYNC_INTERVAL_SECS: u64 = 7;

#[derive(Debug, Default)]
struct InputFilterState {
    terminal_response: Option<TerminalResponseKind>,
    terminal_response_armed_until: Option<Instant>,
}

#[derive(Debug, Default)]
struct UiInteractionState {
    pending_preview: Option<String>,
    input_filter: InputFilterState,
}

#[derive(Debug, Clone, Copy)]
enum TerminalResponseKind {
    Apc,
    Dcs,
    Osc,
    Pm,
    Sos,
}

struct App {
    model: Model,
    tmux: TmuxControl,
    terminal: AppTerminal,
    ui_rx: mpsc::UnboundedReceiver<Event>,
    ui: UiInteractionState,
}

impl App {
    async fn bootstrap(session_name: String) -> Result<(Self, Option<String>)> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
            CursorHide
        )?;
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
                ui: UiInteractionState::default(),
            },
            prev_f_binding,
        ))
    }

    async fn run_loop(&mut self) -> bool {
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

        let mut resync_interval = tokio::time::interval(Duration::from_secs(RESYNC_INTERVAL_SECS));
        resync_interval.reset();
        resync_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                        &mut self.ui,
                        preview_sleep.as_mut(),
                    ).await {
                        break;
                    }
                }

                () = &mut preview_sleep, if self.ui.pending_preview.is_some() => {
                    if !handle_preview_tick(
                        &mut self.model,
                        &mut self.tmux,
                        &mut self.terminal,
                        &mut self.ui.pending_preview,
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

                _ = resync_interval.tick() => {
                    if self.ui.pending_preview.is_none()
                        && !execute_if_any(
                            &mut self.model,
                            &mut self.tmux,
                            &mut self.terminal,
                            vec![Cmd::Resync],
                        ).await
                    {
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

pub async fn run(session_name: String) -> Result<bool> {
    let (mut app, prev_f_binding) = App::bootstrap(session_name).await?;
    let _ = app.run_loop().await;
    let restart = app.model.restart_requested;
    app.shutdown(prev_f_binding.as_deref()).await;
    Ok(restart)
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
        .map(|line| line.trim())
        .find(|line| !line.is_empty() && *line != sidebar_pane_id)
        .unwrap_or("")
        .to_string();
    debug!(home_pane_id, "detected home pane");

    Ok((sidebar_pane_id, sidebar_window_id, home_pane_id))
}

async fn connect_tmux_control(session_name: &str) -> Result<TmuxControl> {
    debug!("connecting to tmux control mode");
    let mut tmux = match TmuxControl::new(session_name).await {
        Ok(tmux) => {
            info!("tmux control connected");
            tmux
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
                        .skip_while(|&word| word != "prefix")
                        .nth(1)
                        == Some("f")
                })
                .map(|line| line.trim().to_string())
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
    execute_commands(model, tmux, terminal, vec![Cmd::PollAiProcesses]).await
}

fn spawn_input_thread(tx: mpsc::UnboundedSender<Event>) {
    std::thread::spawn(move || loop {
        match event::poll(Duration::from_millis(INPUT_POLL_MS)) {
            Ok(true) => match event::read() {
                Ok(event) => {
                    if tx.send(event).is_err() {
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

fn process_ui_event(
    model: &mut Model,
    input_filter: &mut InputFilterState,
    event: Event,
) -> Vec<Cmd> {
    if should_drop_ui_event(input_filter, &event) {
        return Vec::new();
    }

    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            update(model, Msg::Key(key))
        }
        Event::Resize(width, height) => {
            model.terminal_size = (width, height);
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
    input_filter: &mut InputFilterState,
    ui_rx: &mut mpsc::UnboundedReceiver<Event>,
    first_event: Event,
) -> Vec<Cmd> {
    let mut cmds = process_ui_event(model, input_filter, first_event);
    let mut processed_events = 1usize;

    while !contains_layout_helper(&cmds) {
        let Ok(event) = ui_rx.try_recv() else {
            break;
        };
        cmds.extend(process_ui_event(model, input_filter, event));
        processed_events += 1;
    }

    let coalesced = coalesce_commands(cmds);
    debug!(processed_events, cmds = ?coalesced, "ui batch processed");
    coalesced
}

async fn handle_ui_tick(
    model: &mut Model,
    tmux: &mut TmuxControl,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ui_rx: &mut mpsc::UnboundedReceiver<Event>,
    maybe_ui: Option<Event>,
    ui: &mut UiInteractionState,
    preview_sleep: std::pin::Pin<&mut tokio::time::Sleep>,
) -> bool {
    let Some(event) = maybe_ui else {
        warn!("ui channel closed");
        return false;
    };

    let mut cmds = process_ui_batch(model, &mut ui.input_filter, ui_rx, event);
    let ran_layout_helper = contains_layout_helper(&cmds);
    apply_preview_debounce(&mut cmds, &mut ui.pending_preview, preview_sleep);
    let should_continue = execute_if_any(model, tmux, terminal, cmds).await;
    if should_continue && ran_layout_helper {
        arm_terminal_response_filter(&mut ui.input_filter);
        drain_pending_ui_events(ui_rx, &mut ui.input_filter, "layout helper completed");
    }
    should_continue
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
    if let Some(id) = extract_deferred_preview(cmds) {
        *pending_preview = Some(id);
        sleep
            .as_mut()
            .reset(tokio::time::Instant::now() + Duration::from_millis(PREVIEW_DEBOUNCE_MS));
    } else if !cmds.is_empty() {
        *pending_preview = None;
    }
}

fn contains_layout_helper(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|cmd| matches!(cmd, Cmd::ApplyLayoutHelper))
}

fn arm_terminal_response_filter(input_filter: &mut InputFilterState) {
    input_filter.terminal_response_armed_until =
        Some(Instant::now() + Duration::from_millis(TERMINAL_RESPONSE_FILTER_MS));
}

fn should_drop_ui_event(input_filter: &mut InputFilterState, event: &Event) -> bool {
    let Event::Key(key) = event else {
        return false;
    };

    if let Some(kind) = input_filter.terminal_response {
        if !is_terminal_response_filter_armed(input_filter) {
            debug!(?kind, ?key, "terminal response filter expired");
            input_filter.terminal_response = None;
            return false;
        }

        if is_terminal_response_end(key) {
            debug!(?kind, ?key, "dropping terminal response terminator");
            input_filter.terminal_response = None;
        } else {
            debug!(?kind, ?key, "dropping terminal response input");
        }
        return true;
    }

    if !is_terminal_response_filter_armed(input_filter) {
        return false;
    }

    if let Some(kind) = terminal_response_start(key) {
        debug!(?kind, ?key, "dropping terminal response start");
        input_filter.terminal_response = Some(kind);
        input_filter.terminal_response_armed_until =
            Some(Instant::now() + Duration::from_millis(TERMINAL_RESPONSE_FILTER_MS));
        return true;
    }

    false
}

fn is_terminal_response_filter_armed(input_filter: &mut InputFilterState) -> bool {
    let Some(deadline) = input_filter.terminal_response_armed_until else {
        return false;
    };

    if Instant::now() <= deadline {
        true
    } else {
        input_filter.terminal_response_armed_until = None;
        false
    }
}

fn terminal_response_start(key: &KeyEvent) -> Option<TerminalResponseKind> {
    if !key.modifiers.contains(KeyModifiers::ALT) {
        return None;
    }

    match key.code {
        KeyCode::Char('_') => Some(TerminalResponseKind::Apc),
        KeyCode::Char('P') | KeyCode::Char('p') => Some(TerminalResponseKind::Dcs),
        KeyCode::Char(']') => Some(TerminalResponseKind::Osc),
        KeyCode::Char('^') => Some(TerminalResponseKind::Pm),
        KeyCode::Char('X') | KeyCode::Char('x') => Some(TerminalResponseKind::Sos),
        _ => None,
    }
}

fn is_terminal_response_end(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char('\\'))
}

fn drain_pending_ui_events(
    ui_rx: &mut mpsc::UnboundedReceiver<Event>,
    input_filter: &mut InputFilterState,
    reason: &str,
) -> usize {
    let mut dropped = 0usize;
    while let Ok(event) = ui_rx.try_recv() {
        let terminal_response = should_drop_ui_event(input_filter, &event);
        debug!(
            ?event,
            reason = %reason,
            terminal_response,
            "dropping queued ui event"
        );
        dropped += 1;
    }
    if dropped > 0 {
        debug!(dropped, reason = %reason, "dropped queued ui events");
    }
    dropped
}

fn process_tmux_event(model: &mut Model, tmux_event: TmuxEvent) -> Vec<Cmd> {
    match tmux_event {
        TmuxEvent::WindowAdd(_) | TmuxEvent::WindowClose(_) => update(model, Msg::WindowChanged),
        TmuxEvent::WindowRenamed(window_id, name) => {
            update(model, Msg::WindowRenamed { window_id, name })
        }
        TmuxEvent::SessionWindowChanged(ref session_id, window_id)
            if *session_id == model.session_id =>
        {
            update(model, Msg::WindowFocusChanged(window_id))
        }
        TmuxEvent::LayoutChange(window_id) if window_id == model.sidebar.window_id => {
            if let Some(deadline) = model.sidebar.ignore_layout_change_until {
                if std::time::Instant::now() < deadline {
                    vec![Cmd::EnsureSidebarWidth]
                } else {
                    model.sidebar.ignore_layout_change_until = None;
                    vec![Cmd::EnsureSidebarWidth, Cmd::ValidateSidebarPanes]
                }
            } else {
                vec![Cmd::EnsureSidebarWidth, Cmd::ValidateSidebarPanes]
            }
        }
        TmuxEvent::LayoutChange(_) => Vec::new(),
        TmuxEvent::SessionWindowChanged(_, _) => Vec::new(),
        TmuxEvent::SessionChanged(_, _) => vec![Cmd::Resync],
        TmuxEvent::PaneOutput(pane_id) => {
            // High-frequency %output stays on the runtime fast path to avoid
            // pushing every pane redraw through the full Msg -> Cmd cycle.
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
    if model.expire_pending_effects(Instant::now()) {
        cmds.push(Cmd::Resync);
    }
    let mut processed_events = 1;

    while processed_events < MAX_TMUX_EVENTS_PER_BATCH {
        let Ok(event) = tmux_rx.try_recv() else {
            break;
        };
        debug!(?event, "draining tmux event");
        cmds.extend(process_tmux_event(model, event));
        processed_events += 1;
    }

    coalesce_commands(cmds)
}

fn coalesce_commands(cmds: Vec<Cmd>) -> Vec<Cmd> {
    let last_preview_idx = cmds
        .iter()
        .rposition(|cmd| matches!(cmd, Cmd::PreviewWindow { .. }));

    let mut result = Vec::new();
    let mut has_render = false;
    let mut has_resync = false;
    let mut has_ensure_sidebar_width = false;
    let mut has_validate_sidebar_panes = false;

    for (index, cmd) in cmds.into_iter().enumerate() {
        match &cmd {
            Cmd::PreviewWindow { .. } => {
                if Some(index) == last_preview_idx {
                    result.push(cmd);
                }
            }
            Cmd::Render => {
                has_render = true;
            }
            Cmd::ListWindows | Cmd::Resync => {
                if !has_resync {
                    has_resync = true;
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
            _ => result.push(cmd),
        }
    }

    if has_render {
        result.push(Cmd::Render);
    }

    result
}

fn extract_deferred_preview(cmds: &mut Vec<Cmd>) -> Option<String> {
    let dominated_by_cursor = cmds
        .iter()
        .all(|cmd| matches!(cmd, Cmd::PreviewWindow { .. } | Cmd::Render));
    if !dominated_by_cursor {
        return None;
    }

    if let Some(index) = cmds
        .iter()
        .rposition(|cmd| matches!(cmd, Cmd::PreviewWindow { .. }))
    {
        if let Cmd::PreviewWindow { id } = cmds.remove(index) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::model::Mode;
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
        WindowInfo::new(id, index, name, false)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
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

        let mut input_filter = InputFilterState::default();
        let cmds = process_ui_batch(
            &mut model,
            &mut input_filter,
            &mut rx,
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], Cmd::PreviewWindow { ref id } if id == "@3"));
        assert!(matches!(cmds[1], Cmd::Render));
        assert_eq!(model.cursor(), 2);
    }

    #[test]
    fn process_ui_batch_stops_draining_after_layout_helper() {
        let mut model = test_model();
        let _ = update(
            &mut model,
            Msg::WindowListLoaded(vec![window("@1", 1, "tab1")]),
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        )))
        .unwrap();

        let mut input_filter = InputFilterState::default();
        let cmds = process_ui_batch(
            &mut model,
            &mut input_filter,
            &mut rx,
            Event::Key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE)),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::ApplyLayoutHelper));
        assert_eq!(model.mode, Mode::Normal);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn armed_terminal_response_filter_drops_dcs_and_apc_payloads() {
        let mut model = test_model();
        let _ = update(
            &mut model,
            Msg::WindowListLoaded(vec![
                window("@0", 0, "general"),
                window("@44", 44, "target"),
                window("@8", 8, "other"),
            ]),
        );
        model.set_cursor(1);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let events = [
            key(KeyCode::Char('G'), KeyModifiers::SHIFT),
            key(KeyCode::Char('i'), KeyModifiers::NONE),
            key(KeyCode::Char('='), KeyModifiers::NONE),
            key(KeyCode::Char('3'), KeyModifiers::NONE),
            key(KeyCode::Char('1'), KeyModifiers::NONE),
            key(KeyCode::Char(';'), KeyModifiers::NONE),
            key(KeyCode::Char('O'), KeyModifiers::SHIFT),
            key(KeyCode::Char('K'), KeyModifiers::SHIFT),
            key(KeyCode::Char('\\'), KeyModifiers::ALT),
            key(KeyCode::Char('P'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            key(KeyCode::Char('>'), KeyModifiers::NONE),
            key(KeyCode::Char('|'), KeyModifiers::NONE),
            key(KeyCode::Char('W'), KeyModifiers::SHIFT),
            key(KeyCode::Char('e'), KeyModifiers::NONE),
            key(KeyCode::Char('z'), KeyModifiers::NONE),
            key(KeyCode::Char('T'), KeyModifiers::SHIFT),
            key(KeyCode::Char('e'), KeyModifiers::NONE),
            key(KeyCode::Char('r'), KeyModifiers::NONE),
            key(KeyCode::Char('m'), KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char('2'), KeyModifiers::NONE),
            key(KeyCode::Char('\\'), KeyModifiers::ALT),
        ];
        for event in events {
            tx.send(event).unwrap();
        }

        let mut input_filter = InputFilterState::default();
        arm_terminal_response_filter(&mut input_filter);
        let cmds = process_ui_batch(
            &mut model,
            &mut input_filter,
            &mut rx,
            key(KeyCode::Char('_'), KeyModifiers::ALT),
        );

        assert!(cmds.is_empty());
        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(model.cursor(), 1);
        assert!(model.input_buffer.is_empty());
    }

    #[test]
    fn drain_pending_ui_events_clears_queued_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(Event::Key(KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        )))
        .unwrap();
        tx.send(Event::Resize(120, 40)).unwrap();

        let mut input_filter = InputFilterState::default();
        assert_eq!(
            drain_pending_ui_events(&mut rx, &mut input_filter, "test"),
            2,
            "both queued events should be dropped"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn drain_pending_ui_events_tracks_partial_terminal_response() {
        let mut model = test_model();
        let _ = update(
            &mut model,
            Msg::WindowListLoaded(vec![window("@1", 1, "tab1")]),
        );

        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(key(KeyCode::Char('_'), KeyModifiers::ALT)).unwrap();

        let mut input_filter = InputFilterState::default();
        arm_terminal_response_filter(&mut input_filter);

        assert_eq!(
            drain_pending_ui_events(&mut rx, &mut input_filter, "test"),
            1
        );
        assert!(matches!(
            input_filter.terminal_response,
            Some(TerminalResponseKind::Apc)
        ));

        tx.send(key(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        tx.send(key(KeyCode::Char('m'), KeyModifiers::NONE))
            .unwrap();
        tx.send(key(KeyCode::Char('\\'), KeyModifiers::ALT))
            .unwrap();

        let cmds = process_ui_batch(
            &mut model,
            &mut input_filter,
            &mut rx,
            key(KeyCode::Char('2'), KeyModifiers::NONE),
        );

        assert!(cmds.is_empty());
        assert_eq!(model.mode, Mode::Normal);
        assert_eq!(model.cursor(), 0);
        assert!(model.input_buffer.is_empty());
        assert!(input_filter.terminal_response.is_none());
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
        assert!(matches!(cmds[0], Cmd::Resync));
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

    #[test]
    fn process_tmux_batch_is_bounded_under_pane_output_backlog() {
        let mut model = test_model();
        let (tx, mut rx) = mpsc::channel(128);

        for _ in 0..(MAX_TMUX_EVENTS_PER_BATCH + 20) {
            tx.try_send(TmuxEvent::PaneOutput("%ai".to_string()))
                .expect("queue pane output");
        }

        let cmds = process_tmux_batch(
            &mut model,
            &mut rx,
            TmuxEvent::PaneOutput("%ai".to_string()),
        );

        assert!(cmds.is_empty());
        assert_eq!(
            model.ai.output_counts.get("%ai").copied(),
            Some(MAX_TMUX_EVENTS_PER_BATCH as u32)
        );
    }

    #[test]
    fn process_tmux_batch_processes_remaining_queued_events_later() {
        let mut model = test_model();
        let (tx, mut rx) = mpsc::channel(128);

        for _ in 0..MAX_TMUX_EVENTS_PER_BATCH {
            tx.try_send(TmuxEvent::PaneOutput("%ai".to_string()))
                .unwrap();
        }
        tx.try_send(TmuxEvent::WindowAdd("@1".to_string())).unwrap();

        let cmds = process_tmux_batch(
            &mut model,
            &mut rx,
            TmuxEvent::PaneOutput("%ai".to_string()),
        );

        assert!(cmds.is_empty());
        assert_eq!(
            model.ai.output_counts.get("%ai").copied(),
            Some(MAX_TMUX_EVENTS_PER_BATCH as u32)
        );

        let cmds = process_tmux_batch(&mut model, &mut rx, TmuxEvent::WindowAdd("@2".to_string()));

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Resync));
    }

    #[test]
    fn process_tmux_batch_event_behind_many_outputs_is_eventually_processed() {
        let mut model = test_model();
        let (tx, mut rx) = mpsc::channel(256);

        for _ in 0..MAX_TMUX_EVENTS_PER_BATCH {
            tx.try_send(TmuxEvent::PaneOutput("%ai".to_string()))
                .unwrap();
        }
        tx.try_send(TmuxEvent::WindowAdd("@1".to_string())).unwrap();

        let cmds = process_tmux_batch(
            &mut model,
            &mut rx,
            TmuxEvent::PaneOutput("%ai".to_string()),
        );

        assert!(cmds.is_empty());
        assert_eq!(
            model.ai.output_counts.get("%ai").copied(),
            Some(MAX_TMUX_EVENTS_PER_BATCH as u32)
        );

        let cmds = process_tmux_batch(
            &mut model,
            &mut rx,
            TmuxEvent::PaneOutput("%ai".to_string()),
        );

        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Resync));
    }
}
