use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::future::Future;
use std::io::Stdout;
use std::pin::Pin;

use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tracing::{debug, info, warn};

use crate::cmd::Cmd;
use crate::model::{Model, PendingRename, PreviewState};
use crate::msg::Msg;
use crate::tmux::{quote_tmux, TmuxControl, WindowInfo};
use crate::update::update;
use crate::view::render;

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub trait TmuxApi {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
    fn list_windows<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WindowInfo>, String>> + Send + 'a>>;
}

impl TmuxApi for TmuxControl {
    fn send_command<'a>(
        &'a mut self,
        cmd: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            TmuxControl::send_command(self, cmd)
                .await
                .map_err(|err| err.to_string())
        })
    }

    fn list_windows<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<WindowInfo>, String>> + Send + 'a>> {
        Box::pin(async move {
            TmuxControl::list_windows(self)
                .await
                .map_err(|err| err.to_string())
        })
    }
}

pub async fn execute_commands<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    terminal: &mut AppTerminal,
    cmds: Vec<Cmd>,
) -> bool {
    let mut queue: VecDeque<Cmd> = cmds.into();

    while let Some(cmd) = queue.pop_front() {
        match cmd {
            Cmd::PreviewWindow {
                id: target_window_id,
            } => {
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

                let source_window = model.sidebar_window_id.clone();
                // suppress 2 events: join-pane + select-window
                model.ignore_window_changes = 2;
                model.pending_internal_focus_window = Some(target_window_id.clone());

                // Query phase: save target layout and find join target
                remember_window_layout_without_sidebar(model, tmux, &target_window_id).await;
                let target_home =
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await;
                let join_target = if target_home.is_empty() {
                    target_window_id.clone()
                } else {
                    target_home.clone()
                };

                // Action phase: batch all visual tmux operations into a single
                // command so tmux processes them in one server tick (no flicker).
                // -l 30 sets sidebar width at join time to avoid intermediate resize.
                let mut batch = format!(
                    "join-pane -dfhb -l 30 -s {} -t {} ; select-window -t {} ; select-pane -t {}",
                    model.sidebar_pane_id, join_target,
                    target_window_id,
                    model.sidebar_pane_id,
                );
                if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                    write!(batch, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                }

                if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
                    warn!(%err, "preview batch failed");
                    clear_internal_window_change_suppression(model);
                    model.error_message = Some(format!("preview: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Resolve home pane if needed (query after move)
                let new_home = if !target_home.is_empty() {
                    target_home
                } else {
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await
                };

                if !new_home.is_empty() {
                    model.home_pane_id = new_home;
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
                // Suppress %output for next 2 polls to discard pane redraw noise
                model.ai_output_suppress = 2;
                queue.push_front(Cmd::CheckBorder);
            }
            Cmd::RestorePreview => {
                if let PreviewState::Previewing {
                    ref original_window_id,
                    ref original_home_pane_id,
                } = model.preview
                {
                    let orig_window = original_window_id.clone();
                    let orig_home = original_home_pane_id.clone();

                    debug!(orig_window = %orig_window, "restoring preview");

                    // Suppress session-window-changed events from join-pane + select-window
                    model.ignore_window_changes = 2;
                    model.pending_internal_focus_window = Some(orig_window.clone());
                    let source_window = model.sidebar_window_id.clone();
                    remember_window_layout_without_sidebar(model, tmux, &orig_window).await;

                    // Batch: join sidebar back + switch + focus + restore source
                    let mut batch = format!(
                        "join-pane -dfhb -l 30 -s {} -t {} ; select-window -t {} ; select-pane -t {}",
                        model.sidebar_pane_id, orig_home,
                        orig_window,
                        model.sidebar_pane_id,
                    );
                    if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                        write!(batch, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                    }

                    let restored = if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
                        warn!(%err, "restore batch failed, trying fallback");
                        // Fallback: join to window instead of home pane
                        let mut fallback = format!(
                            "join-pane -dfhb -l 30 -s {} -t {} ; select-window -t {} ; select-pane -t {}",
                            model.sidebar_pane_id, orig_window,
                            orig_window,
                            model.sidebar_pane_id,
                        );
                        if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                            write!(fallback, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                        }
                        if let Err(err) = send_batch_with_reconcile(model, tmux, &fallback).await {
                            warn!(%err, "restore fallback batch also failed");
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    };

                    if restored {
                        model.sidebar_window_id = orig_window;
                        // Re-detect home pane: orig_home may be stale (e.g. pane
                        // was closed), especially after the fallback path.
                        let fresh_home = choose_home_pane_in_window(
                            tmux,
                            &model.sidebar_window_id,
                            &model.sidebar_pane_id,
                        )
                        .await;
                        model.home_pane_id = if fresh_home.is_empty() {
                            orig_home
                        } else {
                            fresh_home
                        };
                        model.preview = PreviewState::Home;
                    } else {
                        // Keep reconciled runtime state from failed batch attempts.
                        clear_internal_window_change_suppression(model);
                        if model.sidebar_window_id == orig_window {
                            model.preview = PreviewState::Home;
                        }
                        model.error_message =
                            Some("restore preview failed; state reconciled".to_string());
                    }
                    // Discard %output events from pane redraw during window switch
                    model.ai_output_counts.clear();
                    queue.push_front(Cmd::CheckBorder);
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
                let new_cmd = format!(
                    "new-window -d -n {} -P -F '#{{window_id}}'",
                    quote_tmux(&name)
                );
                match tmux.send_command(&new_cmd).await {
                    Ok(output) => {
                        let window_id = output.trim();
                        if !window_id.is_empty() {
                            let disable_rename = format!(
                                "set-window-option -t {} automatic-rename off ; set-window-option -t {} allow-rename off",
                                window_id, window_id
                            );
                            if let Err(err) = tmux.send_command(&disable_rename).await {
                                warn!(
                                    id = %window_id,
                                    name = %name,
                                    %err,
                                    "failed to disable rename updates for new window"
                                );
                            } else {
                                debug!(
                                    id = %window_id,
                                    "disabled automatic/allow-rename for new window"
                                );
                            }

                            model.pending_renames.insert(
                                window_id.to_string(),
                                PendingRename {
                                    target_name: name.clone(),
                                    observed_count: 0,
                                },
                            );
                            model.pending_rename_last_window_id = Some(window_id.to_string());
                            debug!(
                                id = %window_id,
                                name = %name,
                                "tracking new window for rename stabilization"
                            );
                            queue.push_front(Cmd::Render);
                            queue.push_front(Cmd::PreviewWindow {
                                id: window_id.to_string(),
                            });
                            queue.push_front(Cmd::ListWindows);
                        } else {
                            warn!("new-window returned empty window id");
                        }
                    }
                    Err(err) => {
                        warn!(%err, "new-window failed");
                        model.error_message = Some(format!("new-window: {err}"));
                        queue.push_front(Cmd::Render);
                    }
                }
            }
            Cmd::RenameWindow { id, name } => {
                debug!(id, name, "renaming window");
                let disable_rename = format!(
                    "set-window-option -t {} automatic-rename off ; set-window-option -t {} allow-rename off",
                    id, id
                );
                if let Err(err) = tmux.send_command(&disable_rename).await {
                    warn!(%id, %err, "failed to disable automatic-rename before rename");
                } else {
                    debug!(%id, "disabled rename options before rename");
                }
                let cmd_str = format!("rename-window -t {} {}", id, quote_tmux(&name));
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "rename-window failed");
                    model.error_message = Some(format!("rename-window: {err}"));
                    queue.push_front(Cmd::Render);
                } else {
                    if let Err(err) = tmux
                        .send_command(&disable_rename)
                        .await
                    {
                        warn!(
                            %id,
                            %err,
                            "failed to keep rename updates disabled after rename"
                        );
                    } else {
                        debug!(%id, "kept rename options disabled after rename");
                    }
                    queue.push_front(Cmd::ListWindows);
                }
            }
            Cmd::CloseWindow { id } => {
                // Never kill the window hosting the sidebar
                if model.sidebar_window_id == id {
                    match &model.preview {
                        PreviewState::Previewing { .. } => {
                            if model.close_restore_attempted {
                                // Circuit breaker: already tried restore once, don't loop
                                model.close_restore_attempted = false;
                                warn!(id, "close-after-restore failed, refusing to close sidebar window");
                                model.error_message =
                                    Some("cannot close: sidebar stuck in window".to_string());
                                queue.push_front(Cmd::Render);
                                continue;
                            }
                            model.close_restore_attempted = true;
                            debug!(id, "closing previewed window, restoring first");
                            queue.push_front(Cmd::CloseWindow { id });
                            queue.push_front(Cmd::RestorePreview);
                            continue;
                        }
                        PreviewState::Home => {
                            if model.close_restore_attempted {
                                model.close_restore_attempted = false;
                                warn!(id, "close-after-evacuate failed, refusing to close sidebar window");
                                model.error_message =
                                    Some("cannot close: sidebar stuck in window".to_string());
                                queue.push_front(Cmd::Render);
                                continue;
                            }
                            // Find another window to evacuate sidebar to
                            if let Some(other_id) = model.find_another_window_id(&id) {
                                model.close_restore_attempted = true;
                                debug!(id, other = %other_id, "evacuating sidebar before close");
                                queue.push_front(Cmd::CloseWindow { id });
                                queue.push_front(Cmd::FollowToWindow { window_id: other_id });
                                continue;
                            } else {
                                warn!(id, "no other window to evacuate sidebar to");
                                model.error_message =
                                    Some("cannot close last window".to_string());
                                queue.push_front(Cmd::Render);
                                continue;
                            }
                        }
                    }
                }
                model.close_restore_attempted = false;
                debug!(id, "closing window");
                let cmd_str = format!("kill-window -t {id}");
                if let Err(err) = tmux.send_command(&cmd_str).await {
                    warn!(%err, "kill-window failed");
                    model.error_message = Some(format!("kill-window: {err}"));
                    queue.push_front(Cmd::Render);
                } else {
                    // Proactively refresh — don't rely solely on %window-close notification
                    queue.push_front(Cmd::ListWindows);
                }
            }
            Cmd::FollowToWindow {
                window_id: target_window_id,
            } => {
                if target_window_id == model.sidebar_window_id {
                    continue;
                }

                info!(target = %target_window_id, "following to window");

                let source_window = model.sidebar_window_id.clone();
                // suppress 1 event: join-pane only (no select-window in follow path)
                model.ignore_window_changes = 1;
                model.pending_internal_focus_window = Some(target_window_id.clone());

                // Query phase
                remember_window_layout_without_sidebar(model, tmux, &target_window_id).await;
                let target_home =
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await;
                let join_target = if target_home.is_empty() {
                    target_window_id.clone()
                } else {
                    target_home.clone()
                };

                // Action phase: batch join (+ optional source restore)
                let mut batch = format!(
                    "join-pane -dfhb -l 30 -s {} -t {}",
                    model.sidebar_pane_id, join_target,
                );
                if let Some(layout) = model.layout_without_sidebar_by_window.get(&source_window) {
                    write!(batch, " ; select-layout -t {} {}", source_window, quote_tmux(layout)).unwrap();
                }

                if let Err(err) = send_batch_with_reconcile(model, tmux, &batch).await {
                    warn!(%err, "follow batch failed");
                    clear_internal_window_change_suppression(model);
                    model.error_message = Some(format!("follow: {err}"));
                    queue.push_front(Cmd::Render);
                    continue;
                }

                // Resolve home pane
                let new_home = if !target_home.is_empty() {
                    target_home
                } else {
                    choose_home_pane_in_window(tmux, &target_window_id, &model.sidebar_pane_id)
                        .await
                };

                if !new_home.is_empty() {
                    model.home_pane_id = new_home;
                }

                debug!(
                    sidebar_window = %target_window_id,
                    home_pane = %model.home_pane_id,
                    "follow complete"
                );
                model.sidebar_window_id = target_window_id;
                // Suppress %output for next 2 polls to discard pane redraw noise
                model.ai_output_suppress = 2;
                queue.push_front(Cmd::CheckBorder);
            }
            Cmd::EnsureSidebarWidth => {
                if let Err(err) = tmux
                    .send_command(&format!(
                        "resize-pane -t {} -x 30",
                        model.sidebar_pane_id
                    ))
                    .await
                {
                    warn!(%err, "ensure resize-pane failed");
                }
            }
            Cmd::ValidateSidebarPanes => {
                let pane_list = tmux
                    .send_command(&format!(
                        "list-panes -t {} -F '#{{pane_id}}'",
                        model.sidebar_window_id
                    ))
                    .await
                    .unwrap_or_default();

                let has_content = pane_list
                    .lines()
                    .map(|l| l.trim())
                    .any(|l| !l.is_empty() && l != model.sidebar_pane_id);

                if !has_content {
                    debug!(
                        window = %model.sidebar_window_id,
                        "sidebar window lost all content panes, evacuating"
                    );
                    match &model.preview {
                        PreviewState::Previewing { .. } => {
                            queue.push_front(Cmd::ListWindows);
                            queue.push_front(Cmd::RestorePreview);
                        }
                        PreviewState::Home => {
                            let sidebar_wid = model.sidebar_window_id.clone();
                            if let Some(other_id) = model.find_another_window_id(&sidebar_wid) {
                                queue.push_front(Cmd::ListWindows);
                                queue.push_front(Cmd::FollowToWindow {
                                    window_id: other_id,
                                });
                            }
                        }
                    }
                }
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
            Cmd::PollAiProcesses => {
                // If suppressing output after window switch, discard counts
                if model.ai_output_suppress > 0 {
                    model.ai_output_suppress -= 1;
                    model.ai_output_counts.clear();
                }
                let list_cmd = format!(
                    "list-panes -s -t {} -F '#{{window_id}}\t#{{pane_id}}\t#{{pane_current_command}}\t#{{pane_pid}}'",
                    model.session_name
                );
                match tmux.send_command(&list_cmd).await {
                    Ok(output) => {
                        let candidates = find_ai_pane_candidates(&output, &model.sidebar_pane_id);
                        let (panes, windows) = classify_active_panes(
                            &candidates,
                            &mut model.ai_cpu_tracker,
                            &mut model.ai_output_counts,
                        );
                        let follow_up = update(model, Msg::AiProcessPollResult { panes, windows });
                        for c in follow_up.into_iter().rev() {
                            queue.push_front(c);
                        }
                    }
                    Err(err) => {
                        debug!(%err, "ai process poll failed");
                    }
                }
            }
            Cmd::CheckBorder => {
                let wanted: HashSet<String> = model.ai_panes.clone();
                let current = &model.highlighted_panes;

                // Remove highlight from panes no longer active
                let to_remove: Vec<String> = current.difference(&wanted).cloned().collect();
                for pane_id in &to_remove {
                    let reset_cmd = format!(
                        "set-option -p -t {} -u pane-border-format",
                        pane_id
                    );
                    let _ = tmux.send_command(&reset_cmd).await;
                    debug!(pane = %pane_id, "pane border format reset");
                }

                // Add highlight to newly active panes
                let to_add: Vec<String> = wanted.difference(current).cloned().collect();
                for pane_id in &to_add {
                    let set_cmd = format!(
                        "set-option -p -t {} pane-border-format \" #{{?pane_active,#[fg=yellow#,bold]● #P: #{{pane_current_command}} #{{pane_current_path}},#[fg=yellow]● #P: #{{pane_current_command}}}} \"",
                        pane_id
                    );
                    let _ = tmux.send_command(&set_cmd).await;
                    debug!(pane = %pane_id, "pane border format set to AI active");
                }

                model.highlighted_panes = wanted;
            }
            Cmd::ResetAllBorders => {
                for pane_id in model.highlighted_panes.drain() {
                    let reset_cmd = format!(
                        "set-option -p -t {} -u pane-border-format",
                        pane_id
                    );
                    let _ = tmux.send_command(&reset_cmd).await;
                    debug!(pane = %pane_id, "pane border format reset on cleanup");
                }
            }
            Cmd::Render => {
                if let Err(err) = terminal.draw(|f| render(model, f)) {
                    model.error_message = Some(format!("render: {err}"));
                }
            }
            Cmd::Restart => return false,
            Cmd::Quit => return false,
        }
    }

    true
}

fn clear_internal_window_change_suppression(model: &mut Model) {
    model.ignore_window_changes = 0;
    model.pending_internal_focus_window = None;
}

async fn send_batch_with_reconcile<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    batch: &str,
) -> Result<(), String> {
    if let Err(err) = tmux.send_command(batch).await {
        let old_window = model.sidebar_window_id.clone();
        let old_home = model.home_pane_id.clone();
        warn!(
            %err,
            old_window = %old_window,
            old_home = %old_home,
            batch = %batch,
            "reconcile: batch failed; syncing state"
        );
        // A semicolon-joined tmux command can fail after partially applying earlier
        // subcommands. Re-sync model with tmux before returning the error.
        reconcile_sidebar_state(model, tmux).await;
        warn!(
            new_window = %model.sidebar_window_id,
            new_home = %model.home_pane_id,
            "reconcile: state synced after batch failure"
        );
        return Err(err);
    }
    Ok(())
}

async fn reconcile_sidebar_state<T: TmuxApi>(model: &mut Model, tmux: &mut T) {
    let mut window_updated = false;
    let current_window = tmux
        .send_command(&format!(
            "display-message -t {} -p '#{{window_id}}'",
            model.sidebar_pane_id
        ))
        .await
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|v| !v.is_empty());

    if let Some(window_id) = current_window {
        if model.sidebar_window_id != window_id {
            info!(
                old = %model.sidebar_window_id,
                new = %window_id,
                "reconcile: sidebar window id updated"
            );
        }
        model.sidebar_window_id = window_id;
        window_updated = true;
    }

    let new_home =
        choose_home_pane_in_window(tmux, &model.sidebar_window_id, &model.sidebar_pane_id).await;
    if !new_home.is_empty() {
        if model.home_pane_id != new_home {
            info!(
                old = %model.home_pane_id,
                new = %new_home,
                window = %model.sidebar_window_id,
                "reconcile: home pane id updated"
            );
        }
        model.home_pane_id = new_home;
    } else if window_updated {
        warn!(
            window = %model.sidebar_window_id,
            "reconcile: could not determine non-sidebar home pane"
        );
    }
}

async fn choose_home_pane_in_window<T: TmuxApi>(
    tmux: &mut T,
    window_id: &str,
    sidebar_pane_id: &str,
) -> String {
    let pane_list = tmux
        .send_command(&format!(
            "list-panes -t {} -F '#{{pane_id}}\t#{{pane_active}}'",
            window_id
        ))
        .await
        .unwrap_or_default();

    let mut first_non_sidebar = String::new();
    for line in pane_list.lines() {
        let mut parts = line.split('\t');
        let pane_id = parts.next().unwrap_or("").trim();
        let active = parts.next().unwrap_or("").trim();
        if pane_id.is_empty() || pane_id == sidebar_pane_id {
            continue;
        }
        if first_non_sidebar.is_empty() {
            first_non_sidebar = pane_id.to_string();
        }
        if active == "1" {
            return pane_id.to_string();
        }
    }

    first_non_sidebar
}

async fn read_window_layout<T: TmuxApi>(tmux: &mut T, window_id: &str) -> Option<String> {
    if window_id.is_empty() {
        return None;
    }
    let out = tmux
        .send_command(&format!(
            "display-message -t {} -p '#{{window_layout}}'",
            window_id
        ))
        .await
        .ok()?;
    let layout = out.trim();
    if layout.is_empty() {
        None
    } else {
        Some(layout.to_string())
    }
}

const AI_PROCESS_NAMES: &[&str] = &["claude", "codex", "gemini"];

struct AiPaneCandidate {
    pane_id: String,
    window_id: String,
    pane_pid: u32,
}

/// Parse tmux list-panes output to find panes running AI processes.
fn find_ai_pane_candidates(output: &str, sidebar_pane_id: &str) -> Vec<AiPaneCandidate> {
    let mut candidates = Vec::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let window_id = parts[0].trim();
        let pane_id = parts[1].trim();
        let command = parts[2].trim();
        let pid_str = parts[3].trim();

        if pane_id == sidebar_pane_id {
            continue;
        }

        let lower = command.to_lowercase();
        if !AI_PROCESS_NAMES.iter().any(|name| lower.contains(name)) {
            continue;
        }

        if let Ok(pane_pid) = pid_str.parse::<u32>() {
            candidates.push(AiPaneCandidate {
                pane_id: pane_id.to_string(),
                window_id: window_id.to_string(),
                pane_pid,
            });
        }
    }
    candidates
}

/// Number of consecutive idle polls before demoting an AI pane to inactive.
/// At 500ms poll interval, 6 polls = 3 seconds of grace after last activity.
/// Grace covers brief pauses between streaming chunks and tool calls.
/// Thinking phases sustain grace via CPU (delta >= MIN_CPU_DELTA_FOR_GRACE).
const AI_CPU_GRACE_POLLS: u16 = 6;

/// Minimum %output events per poll interval to count as streaming.
/// Streaming generates 6-100+ events/s; typing noise is typically 1-4/s
/// with occasional spikes to 6-8 during fast English input.
const MIN_OUTPUT_BURST: u32 = 6;

/// Consecutive polls with output bursts required before activating a pane.
/// Prevents false activation from typing in TUI apps (e.g. Claude Code input
/// causes brief %output bursts from UI redraws, but doesn't sustain across polls).
/// AI streaming sustains bursts across many consecutive polls.
const MIN_CONSECUTIVE_BURSTS: u8 = 2;

/// Minimum CPU delta (ticks/poll) required to sustain grace period.
/// Must be high enough to filter out warm-idle background CPU (0-15 typical,
/// spikes to ~50). Active thinking/tool execution drives 100+ consistently.
const MIN_CPU_DELTA_FOR_GRACE: u64 = 50;

/// Enable verbose per-pane logging in classify_active_panes.
const AI_DEBUG: bool = false;

/// Maximum depth when walking the process tree to sum CPU ticks.
/// Prevents runaway traversal on deeply nested process hierarchies.
const MAX_PROC_TREE_DEPTH: u8 = 8;

/// Determine which AI panes are actively working using %output as the
/// primary signal and CPU as a secondary grace-sustaining signal.
///
/// Design principle: **%output-primary, CPU-secondary**
/// - Only sustained %output bursts can ACTIVATE a pane (idle → active)
/// - Activation requires MIN_CONSECUTIVE_BURSTS consecutive polls with bursts
///   to filter out brief UI redraw noise from typing in TUI apps
/// - CPU activity can only SUSTAIN an already-active pane during grace
/// - This prevents warm idle sessions (high background CPU) from false-activating
///
/// Activation: consecutive polls with burst >= MIN_CONSECUTIVE_BURSTS
/// Grace sustain: CPU delta >= MIN_CPU_DELTA_FOR_GRACE (still computing)
/// Grace expiry: neither signal for AI_CPU_GRACE_POLLS consecutive polls
///
/// Output counts are reset after each call (consumed per poll).
/// Returns (active_pane_ids, active_window_ids).
fn classify_active_panes(
    candidates: &[AiPaneCandidate],
    prev_cpu: &mut HashMap<String, (u32, u64, u16, u8)>,
    output_counts: &mut HashMap<String, u32>,
) -> (HashSet<String>, HashSet<String>) {
    let mut active_panes = HashSet::new();
    let mut active_windows = HashSet::new();
    let mut seen_panes = HashSet::new();

    for c in candidates {
        seen_panes.insert(c.pane_id.clone());

        let ai_pid = find_ai_child_pid(c.pane_pid).unwrap_or(c.pane_pid);
        let current_cpu = read_cpu_ticks_tree(ai_pid).unwrap_or(0);

        let output_count = output_counts.get(&c.pane_id).copied().unwrap_or(0);
        let is_burst = output_count >= MIN_OUTPUT_BURST;

        // polls_since_active semantics:
        //   0 = never been active / fully idle
        //   1 = active right now (just activated or reactivated by streaming)
        //   2..=GRACE = was active, grace period counting down
        //   >GRACE = demoted to idle
        // consecutive_bursts: how many consecutive polls had output bursts.
        //   Activation requires >= MIN_CONSECUTIVE_BURSTS to filter out
        //   brief UI redraw noise from typing in TUI apps.
        let (is_active, new_polls, new_bursts) =
            if let Some(&(prev_pid, prev_ticks, polls, bursts)) = prev_cpu.get(&c.pane_id) {
                if prev_pid != ai_pid {
                    // PID changed — reset tracking
                    let b = if is_burst { 1 } else { 0 };
                    let activated = b >= MIN_CONSECUTIVE_BURSTS;
                    (activated, if activated { 1 } else { 0 }, b)
                } else if is_burst {
                    let b = bursts.saturating_add(1);
                    if polls >= 1 {
                        // Already active — sustained streaming resets grace
                        (true, 1, b)
                    } else if b >= MIN_CONSECUTIVE_BURSTS {
                        // Was idle, now enough consecutive bursts to activate
                        (true, 1, b)
                    } else {
                        // Burst seen but not enough consecutive ones yet
                        (false, 0, b)
                    }
                } else if polls == 0 {
                    (false, 0, 0)
                } else if polls <= AI_CPU_GRACE_POLLS {
                    let cpu_delta = current_cpu.saturating_sub(prev_ticks);
                    if cpu_delta >= MIN_CPU_DELTA_FOR_GRACE {
                        (true, 1, 0)
                    } else {
                        let new_polls = polls.saturating_add(1);
                        (new_polls <= AI_CPU_GRACE_POLLS, new_polls, 0)
                    }
                } else {
                    (false, 0, 0)
                }
            } else {
                // First time seeing this pane — start counting
                let b = if is_burst { 1 } else { 0 };
                (false, 0, b)
            };

        if AI_DEBUG {
            let prev_info = prev_cpu.get(&c.pane_id);
            let prev_ticks = prev_info.map(|&(_, t, _, _)| t).unwrap_or(0);
            let prev_polls = prev_info.map(|&(_, _, p, _)| p).unwrap_or(0);
            let prev_bursts = prev_info.map(|&(_, _, _, b)| b).unwrap_or(0);
            let cpu_delta = current_cpu.saturating_sub(prev_ticks);
            let single_cpu = read_cpu_ticks(ai_pid).unwrap_or(0);
            let single_delta = single_cpu.saturating_sub(prev_ticks);
            debug!(
                pane = %c.pane_id,
                window = %c.window_id,
                ai_pid,
                cpu_delta,
                single_delta,
                output_count,
                is_burst,
                prev_polls,
                new_polls,
                prev_bursts,
                new_bursts,
                is_active,
                "ai classify"
            );
        }

        prev_cpu.insert(c.pane_id.clone(), (ai_pid, current_cpu, new_polls, new_bursts));

        if is_active {
            active_panes.insert(c.pane_id.clone());
            active_windows.insert(c.window_id.clone());
        }
    }

    prev_cpu.retain(|k, _| seen_panes.contains(k));
    output_counts.clear();

    (active_panes, active_windows)
}

/// Walk /proc to find the AI process that is a child of the pane's shell.
fn find_ai_child_pid(pane_pid: u32) -> Option<u32> {
    let children_path = format!("/proc/{}/task/{}/children", pane_pid, pane_pid);
    let children = std::fs::read_to_string(&children_path).ok()?;
    for child_str in children.split_whitespace() {
        let child_pid: u32 = child_str.parse().ok()?;
        let comm_path = format!("/proc/{}/comm", child_pid);
        if let Ok(comm) = std::fs::read_to_string(&comm_path) {
            let lower = comm.trim().to_lowercase();
            if AI_PROCESS_NAMES.iter().any(|name| lower.contains(name)) {
                return Some(child_pid);
            }
        }
    }
    None
}

/// Read CPU time (utime + stime) from /proc/<pid>/stat.
fn read_cpu_ticks(pid: u32) -> Option<u64> {
    let path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(&path).ok()?;
    // comm field can contain spaces/parens, so find last ')' first
    let after_comm = content.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After ')': [0]=state [1]=ppid ... [11]=utime [12]=stime
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Read aggregate CPU time (utime + stime) across the entire process tree
/// rooted at `pid`. Walks descendants via `/proc/PID/task/TID/children` to
/// capture CPU from subprocesses (e.g. Claude Code spawning multiple subagents).
fn read_cpu_ticks_tree(pid: u32) -> Option<u64> {
    let root_cpu = read_cpu_ticks(pid)?;
    let mut total = root_cpu;

    // Iterative BFS with depth limit
    let mut stack: Vec<(u32, u8)> = vec![(pid, 0)];
    while let Some((current_pid, depth)) = stack.pop() {
        if depth >= MAX_PROC_TREE_DEPTH {
            continue;
        }
        // Read children from all TIDs of this process (multi-threaded processes
        // may have children attached to non-main threads)
        let task_dir = format!("/proc/{}/task", current_pid);
        let tasks = match std::fs::read_dir(&task_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for task_entry in tasks {
            let tid = match task_entry {
                Ok(e) => e.file_name(),
                Err(_) => continue,
            };
            let children_path =
                format!("/proc/{}/task/{}/children", current_pid, tid.to_string_lossy());
            let children = match std::fs::read_to_string(&children_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for child_str in children.split_whitespace() {
                if let Ok(child_pid) = child_str.parse::<u32>() {
                    if let Some(child_cpu) = read_cpu_ticks(child_pid) {
                        total += child_cpu;
                    }
                    stack.push((child_pid, depth + 1));
                }
            }
        }
    }
    Some(total)
}

async fn remember_window_layout_without_sidebar<T: TmuxApi>(
    model: &mut Model,
    tmux: &mut T,
    window_id: &str,
) {
    if let Some(layout) = read_window_layout(tmux, window_id).await {
        model
            .layout_without_sidebar_by_window
            .insert(window_id.to_string(), layout);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakeTmux {
        responses: VecDeque<Result<String, String>>,
        commands: Vec<String>,
    }

    impl FakeTmux {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                responses: responses.into(),
                commands: Vec::new(),
            }
        }
    }

    fn test_model() -> Model {
        Model::new(
            "s".to_string(),
            "$1".to_string(),
            "%sidebar".to_string(),
            "%home_old".to_string(),
            "@old".to_string(),
        )
    }

    impl TmuxApi for FakeTmux {
        fn send_command<'a>(
            &'a mut self,
            cmd: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.commands.push(cmd.to_string());
                self.responses
                    .pop_front()
                    .expect("missing fake response for command")
            })
        }

        fn list_windows<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<WindowInfo>, String>> + Send + 'a>> {
            Box::pin(async move { Err("not used in this test".to_string()) })
        }
    }

    #[test]
    fn find_ai_pane_candidates_filters_correctly() {
        let output = "@1\t%10\tclaude\t1000\n@1\t%11\tzsh\t1001\n@2\t%20\tgemini\t1002\n@2\t%21\tcodex\t1003\n";
        let candidates = find_ai_pane_candidates(output, "%sidebar");
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].pane_id, "%10");
        assert_eq!(candidates[0].window_id, "@1");
        assert_eq!(candidates[1].pane_id, "%20");
        assert_eq!(candidates[1].window_id, "@2");
        assert_eq!(candidates[2].pane_id, "%21");
    }

    #[test]
    fn find_ai_pane_candidates_skips_sidebar() {
        let output = "@1\t%sidebar\tclaude\t1000\n@1\t%10\tclaude\t1001\n";
        let candidates = find_ai_pane_candidates(output, "%sidebar");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].pane_id, "%10");
    }

    #[test]
    fn classify_active_panes_first_seen_is_idle() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(!panes.contains("%10"), "first-seen pane should be idle (baseline)");
        assert!(prev.contains_key("%10"), "baseline should be recorded");
    }

    #[test]
    fn classify_active_panes_high_cpu_alone_does_not_activate() {
        // Key test: CPU alone should NEVER activate a pane.
        // This prevents warm idle sessions from showing as active.
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=0 (never active), even with high CPU — should stay idle
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 0u16, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(!panes.contains("%10"), "CPU alone should never activate (output-primary)");
    }

    #[test]
    fn classify_active_panes_single_burst_does_not_activate_first_seen() {
        // Single burst should NOT activate — needs consecutive bursts to
        // filter out typing noise in TUI apps like Claude Code.
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "single burst should not activate first-seen pane");
        assert!(counts.is_empty(), "counts should be reset after poll");
        let &(_, _, _, bursts) = prev.get("%10").unwrap();
        assert_eq!(bursts, 1, "should have recorded 1 consecutive burst");
    }

    #[test]
    fn classify_active_panes_consecutive_bursts_activate() {
        // Consecutive bursts (sustained streaming) should activate.
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let mut counts = HashMap::new();

        // First burst — not yet active
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "first burst should not activate");

        // Second burst — now activate
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(panes.contains("%10"), "consecutive bursts should activate pane");
        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(polls, 1, "polls should be 1 after activation");
    }

    #[test]
    fn classify_active_panes_single_burst_does_not_activate_existing() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Existing pane with baseline (polls=0, never active)
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 0u16, 0u8));

        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "single burst should not activate idle pane");
        let &(_, _, polls, bursts) = prev.get("%10").unwrap();
        assert_eq!(polls, 0, "should stay idle");
        assert_eq!(bursts, 1, "should record 1 consecutive burst");
    }

    #[test]
    fn classify_active_panes_burst_gap_resets_counter() {
        // Burst → no burst → burst should NOT activate (counter resets)
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        let mut counts = HashMap::new();

        // First burst
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"));

        // Gap (no burst)
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"));
        let &(_, _, _, bursts) = prev.get("%10").unwrap();
        assert_eq!(bursts, 0, "burst counter should reset on gap");

        // Another burst — starts counting from 1 again
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "should not activate after gap");
    }

    #[test]
    fn classify_active_panes_output_burst_resets_grace() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Was active (polls >= 1), grace is about to expire
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, AI_CPU_GRACE_POLLS, 0u8));

        // Output burst on already-active pane should reset grace (no consecutive requirement)
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST + 10);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(panes.contains("%10"), "output burst should reset grace timer");
        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(polls, 1, "polls should reset to 1");
    }

    #[test]
    fn classify_active_panes_grace_with_cpu_stays_active() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=1 means just activated by streaming, now no output but CPU still working
        // Since fake PID won't have /proc entry, cpu_delta=0, so grace counts down
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 1u16, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        // With fake PID, cpu_delta=0, so grace counts down (polls 1→2)
        // But still within grace period
        assert!(panes.contains("%10"), "should stay active within grace period");

        let &(_, _, polls, _) = prev.get("%10").unwrap();
        assert_eq!(polls, 2, "polls counter should increment (no CPU activity from fake pid)");
    }

    #[test]
    fn classify_active_panes_grace_expires_to_idle() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls at grace limit — next poll should demote to idle
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, AI_CPU_GRACE_POLLS, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(!panes.contains("%10"), "should be idle after grace period expires");
    }

    #[test]
    fn classify_active_panes_stays_active_during_thinking() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // polls=2 means was active, been quiet for 2 polls — still within 3s grace
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, 2u16, 0u8));

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(panes.contains("%10"), "should stay active during thinking (within grace)");
    }

    #[test]
    fn classify_active_panes_cleans_stale_entries() {
        let candidates = vec![]; // no AI panes
        let mut prev = HashMap::new();
        prev.insert("%gone".to_string(), (123u32, 50000u64, 0u16, 0u8));

        let _ = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(prev.is_empty(), "stale entries should be cleaned");
    }

    #[test]
    fn classify_active_panes_low_output_not_streaming() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        let mut prev = HashMap::new();
        // Low output count (idle terminal noise) should NOT activate
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST - 1);

        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "low output count should not activate");
    }

    #[test]
    fn classify_active_panes_grace_expired_needs_consecutive_to_reactivate() {
        let candidates = vec![AiPaneCandidate {
            pane_id: "%10".to_string(),
            window_id: "@1".to_string(),
            pane_pid: 99999,
        }];
        // Grace fully expired (polls > GRACE)
        let mut prev = HashMap::new();
        prev.insert("%10".to_string(), (99999u32, 50000u64, AI_CPU_GRACE_POLLS + 1, 0u8));

        // No streaming — should demote to idle
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut HashMap::new());
        assert!(!panes.contains("%10"), "grace-expired pane should demote to idle");

        // Single burst — starts counting but doesn't activate yet
        let mut counts = HashMap::new();
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(!panes.contains("%10"), "single burst after grace expiry should not reactivate");

        // Second consecutive burst — now reactivate
        counts.insert("%10".to_string(), MIN_OUTPUT_BURST);
        let (panes, _) = classify_active_panes(&candidates, &mut prev, &mut counts);
        assert!(panes.contains("%10"), "consecutive bursts should reactivate grace-expired pane");
    }

    #[test]
    fn read_cpu_ticks_parses_stat() {
        // Test with a synthetic /proc/pid/stat line
        // We can't easily unit-test read_cpu_ticks since it reads /proc,
        // but we can verify it works on our own process
        let pid = std::process::id();
        let result = read_cpu_ticks(pid);
        assert!(result.is_some(), "should be able to read own process CPU ticks");
    }

    #[test]
    fn read_cpu_ticks_tree_includes_own_process() {
        let pid = std::process::id();
        let single = read_cpu_ticks(pid).unwrap();
        let tree = read_cpu_ticks_tree(pid).unwrap();
        assert!(tree >= single, "tree CPU ({tree}) should be >= single process CPU ({single})");
    }

    #[test]
    fn read_cpu_ticks_tree_nonexistent_pid() {
        // PID 4_000_000 should not exist
        let result = read_cpu_ticks_tree(4_000_000);
        assert!(result.is_none(), "nonexistent PID should return None");
    }

    #[test]
    fn read_cpu_ticks_tree_with_children() {
        use std::process::Command;
        // Spawn a child process that does some work
        let child = Command::new("sleep").arg("0.1").spawn();
        if let Ok(mut child) = child {
            let pid = std::process::id();
            let single = read_cpu_ticks(pid).unwrap();
            let tree = read_cpu_ticks_tree(pid).unwrap();
            assert!(
                tree >= single,
                "tree CPU ({tree}) with child should be >= single ({single})"
            );
            let _ = child.wait();
        }
    }

    #[tokio::test]
    async fn batch_failure_reconciles_sidebar_state() {
        let mut model = test_model();

        let mut tmux = FakeTmux::new(vec![
            Err("batch failed".to_string()),
            Ok("@new\n".to_string()),
            Ok("%sidebar\t1\n%home_new\t0\n".to_string()),
        ]);

        let err = send_batch_with_reconcile(&mut model, &mut tmux, "join-pane ; resize-pane")
            .await
            .expect_err("batch should fail");

        assert_eq!(err, "batch failed");
        assert_eq!(model.sidebar_window_id, "@new");
        assert_eq!(model.home_pane_id, "%home_new");
        assert_eq!(tmux.commands.len(), 3);
        assert!(tmux.commands[1].contains("display-message -t %sidebar -p '#{window_id}'"));
        assert!(tmux.commands[2].contains("list-panes -t @new"));
    }
}
